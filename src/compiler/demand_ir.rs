//! Keep one checked rustc session alive while the guest requests monomorphized IR.
use std::cell::Cell;
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::io::{self, BufWriter, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode};

use rustc_codegen_llvm::LlvmCodegenBackend;
use rustc_codegen_ssa::traits::{
    CodegenBackend, ExtraBackendMethods, ModuleBufferMethods, WriteBackendMethods,
};
use rustc_data_structures::fingerprint::Fingerprint;
use rustc_data_structures::stable_hash::{StableHash, StableHasher};
use rustc_driver::{Callbacks, Compilation};
use rustc_hir::attrs::Linkage;
use rustc_hir::def::DefKind;
use rustc_hir::def_id::LOCAL_CRATE;
use rustc_interface::interface::{Compiler, Config};
use rustc_middle::middle::codegen_fn_attrs::CodegenFnAttrFlags;
use rustc_middle::mono::{CodegenUnit, CollectionMode, MonoItem, MonoItemData, Visibility};
use rustc_middle::ty::{Instance, TyCtxt, TypingEnv, Unnormalized};
use rustc_middle::util::Providers;
use rustc_session::Session;
use rustc_session::config::{OutputType, OutputTypes};
use rustc_span::{DUMMY_SP, Symbol};

use super::cargo_demand::Recipe;
use super::demand_analysis::{self, report};
use super::demand_cache::Cache;
use super::{NATIVE_RUSTC, SYSROOT};

unsafe extern "C" {
    fn dup2(source: i32, target: i32) -> i32;
    fn fcntl(fd: i32, command: i32, ...) -> i32;
}

thread_local! {
    static ACTIVE_UNIT: Cell<*const ()> = const { Cell::new(std::ptr::null()) };
}

struct UnitGuard;
impl Drop for UnitGuard {
    fn drop(&mut self) {
        ACTIVE_UNIT.set(std::ptr::null());
    }
}

fn install(_: &Session, providers: &mut Providers) {
    // This path never invokes rustc's whole-crate mono collector. The LLVM
    // declaration visibility query only needs to distinguish local definitions.
    providers.queries.is_codegened_item = |_, id| id.is_local();
    providers.queries.collect_and_partition_mono_items = |tcx, ()| {
        tcx.dcx()
            .fatal("naspeed demand IR: unexpected whole-crate codegen query")
    };
    providers.queries.codegen_unit = |_, name| {
        let pointer = ACTIVE_UNIT.get().cast::<CodegenUnit<'_>>();
        assert!(!pointer.is_null());
        // SAFETY: this provider is called synchronously on the compiler thread
        // during compile_codegen_unit. The pinned compiler's arena owns the
        // unit for the entire TyCtxt lifetime, including in the query cache.
        let unit = unsafe { &*pointer };
        assert_eq!(unit.name(), name);
        unit
    };
}

fn configure(config: &mut Config, incremental: Option<PathBuf>) {
    // Run normal type, borrow and lint checks, but do not eagerly force the
    // non-const MIR elaboration that rustc runs before ordinary codegen.
    config.opts.output_types = OutputTypes::new(&[(OutputType::Metadata, None)]);
    config.override_queries = Some(install);
    config.opts.incremental = incremental;
    config.opts.cg.prefer_dynamic = true;
    config.opts.cg.codegen_units = Some(1);
    config.opts.cli_forced_codegen_units = Some(1);
}

struct Driver {
    args: Vec<OsString>,
    result: ExitCode,
}

impl Callbacks for Driver {
    fn config(&mut self, config: &mut Config) {
        configure(config, None);
    }

    fn after_analysis<'tcx>(&mut self, _: &Compiler, tcx: TyCtxt<'tcx>) -> Compilation {
        tcx.dcx().abort_if_errors();
        self.result = exit_code(run_guest(tcx, &self.args));
        Compilation::Stop
    }
}

struct CargoDriver {
    result: ExitCode,
    recipe: Recipe,
    generation: PathBuf,
    incremental: PathBuf,
}

impl Callbacks for CargoDriver {
    fn config(&mut self, config: &mut Config) {
        configure(config, Some(std::mem::take(&mut self.incremental)));
        config.opts.unstable_opts.query_dep_graph |= report();
    }

    fn after_crate_root_parsing(
        &mut self,
        compiler: &Compiler,
        krate: &mut rustc_ast::Crate,
    ) -> Compilation {
        self.result = exit_code(
            demand_analysis::run_incremental(compiler, krate, |tcx| {
                demand_analysis::check(tcx, "worker");
                self.recipe.verify_expansion(tcx)?;
                let mut cache = Cache::new(std::mem::take(&mut self.generation))?;
                // SAFETY: posix_spawn gave the compiler sole ownership of fd 3.
                let mut wire = unsafe { UnixStream::from_raw_fd(3) };
                serve(tcx, &mut wire, Some(&mut cache))?;
                Ok(cache.into_work_products())
            })
            .map(|()| ExitCode::SUCCESS),
        );
        Compilation::Stop
    }
}

fn exit_code(result: io::Result<ExitCode>) -> ExitCode {
    match result {
        Ok(status) => status,
        Err(error) => {
            eprintln!("naspeed demand IR: {error}");
            ExitCode::from(125)
        }
    }
}

pub fn run(mut args: impl Iterator<Item = OsString>) -> ExitCode {
    if !cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        eprintln!("naspeed demand IR requires Apple Silicon macOS");
        return ExitCode::FAILURE;
    }
    let Some(source) = args.next() else {
        eprintln!("usage: ns run --lazy-ir SOURCE.rs [-- ARGS...]");
        return ExitCode::FAILURE;
    };
    let source_path = Path::new(&source);
    let source = match source_path.canonicalize() {
        Ok(path) if path.is_file() && path.extension() == Some(OsStr::new("rs")) => path,
        _ => {
            eprintln!("naspeed demand IR: expected a Rust source file");
            return ExitCode::FAILURE;
        }
    };
    let Some(source) = source.to_str() else {
        eprintln!("naspeed demand IR: source path must be UTF-8");
        return ExitCode::FAILURE;
    };
    if args.next().is_some_and(|arg| arg != "--") {
        eprintln!("naspeed demand IR: pass program arguments after --");
        return ExitCode::FAILURE;
    }
    let mut driver = Driver {
        args: std::iter::once(OsString::from(source))
            .chain(args)
            .collect(),
        result: ExitCode::FAILURE,
    };
    let argv = [
        NATIVE_RUSTC,
        source,
        "--edition=2024",
        "--crate-name=naspeed_lazy_guest",
        "--sysroot",
        SYSROOT,
        "-Cprefer-dynamic",
        "-Ccodegen-units=1",
    ]
    .map(str::to_owned);
    let early_dcx = rustc_session::EarlyDiagCtxt::new(Default::default());
    rustc_driver::init_rustc_env_logger(&early_dcx);
    rustc_driver::catch_with_exit_code(|| {
        rustc_driver::run_compiler(&argv, &mut driver);
        driver.result
    })
}

pub fn run_recipe(path: Option<OsString>) -> ExitCode {
    let Some(path) = path else {
        return ExitCode::FAILURE;
    };
    let mut recipe = match Recipe::open(Path::new(&path)).and_then(|recipe| {
        recipe.enter()?;
        Ok(recipe)
    }) {
        Ok(recipe) => recipe,
        Err(err) => {
            eprintln!("naspeed demand IR: {err}");
            return ExitCode::from(125);
        }
    };
    if report() {
        eprintln!("naspeed demand compiler: started");
    }
    let argv = std::mem::take(&mut recipe.argv);
    let generation = Path::new(&path).parent().unwrap();
    let root = generation.parent().unwrap();
    let incremental = root.join("demand-incremental").join(&recipe.namespace);
    let mut driver = CargoDriver {
        result: ExitCode::FAILURE,
        recipe,
        generation: root.join("ir").join(generation.file_name().unwrap()),
        incremental,
    };
    let early_dcx = rustc_session::EarlyDiagCtxt::new(Default::default());
    rustc_driver::init_rustc_env_logger(&early_dcx);
    rustc_driver::catch_with_exit_code(|| {
        rustc_driver::run_compiler(&argv, &mut driver);
        driver.result
    })
}

pub fn validate(tcx: TyCtxt<'_>) -> io::Result<()> {
    if tcx.has_global_allocator(LOCAL_CRATE) || !tcx.native_libraries(LOCAL_CRATE).is_empty() {
        return Err(io::Error::other(
            "custom allocators and native libraries are not supported",
        ));
    }
    for id in tcx.hir_crate_items(()).definitions() {
        match tcx.def_kind(id) {
            DefKind::GlobalAsm => return Err(io::Error::other("global assembly is unsupported")),
            DefKind::Fn | DefKind::AssocFn | DefKind::Static { .. } => {
                let attrs = tcx.codegen_fn_attrs(id);
                if attrs.contains_extern_indicator()
                    || attrs.link_section.is_some()
                    || attrs.flags.intersects(
                        CodegenFnAttrFlags::USED_COMPILER | CodegenFnAttrFlags::USED_LINKER,
                    )
                {
                    return Err(io::Error::other(
                        "native exports and retained or custom-section items are unsupported",
                    ));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn run_guest(tcx: TyCtxt<'_>, args: &[OsString]) -> io::Result<ExitCode> {
    validate(tcx)?;
    let mut std = None;
    for &id in tcx.crates(()) {
        let source = tcx.used_crate_source(id);
        if source.paths().any(|p| !p.starts_with(SYSROOT)) {
            return Err(io::Error::other(
                "only dependencies from the pinned Rust SDK are supported",
            ));
        }
        if tcx.crate_name(id).as_str() == "std" {
            // Metadata-only sessions can select an .rmeta instead of seeing
            // the dylib. Resolve the matching std hash, not any libstd file.
            std = source.paths().next().map(|p| p.with_extension("dylib"));
        }
    }
    let std = std
        .filter(|p| p.is_file())
        .ok_or_else(|| io::Error::other("the pinned dynamic Rust std is required"))?;
    let (mut server, client) = UnixStream::pair()?;
    let fd = client.as_raw_fd();
    let launcher = std::env::current_exe()?.with_file_name("naspeed-lazy-launch");
    let mut child = Command::new(launcher);
    child.arg("--demand-ir").arg(std).args(args);
    // SAFETY: dup2/fcntl are async-signal-safe. FD 3 is only inherited by
    // the launcher, never used to transport the guest's standard streams.
    unsafe {
        child.pre_exec(move || {
            if dup2(fd, 3) == -1 || fcntl(3, 2, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = Guest(child.spawn()?);
    drop(client);
    serve(tcx, &mut server, None)?;
    drop(server);
    let status = child.0.wait()?;
    Ok(ExitCode::from(
        status
            .code()
            .unwrap_or_else(|| 128 + status.signal().unwrap_or(1))
            .clamp(0, 255) as u8,
    ))
}

fn serve(tcx: TyCtxt<'_>, wire: &mut UnixStream, cache: Option<&mut Cache>) -> io::Result<()> {
    let entry = entry_points(tcx)?;
    let parent_backend = LlvmCodegenBackend::new();
    // SAFETY: this pinned SDK's LlvmCodegenBackend::new returns a boxed
    // LlvmCodegenBackend as its CodegenBackend trait object.
    let backend =
        unsafe { &*((&*parent_backend as *const dyn CodegenBackend).cast::<LlvmCodegenBackend>()) };
    Producer {
        tcx,
        backend,
        cache,
        items: HashMap::new(),
        report: report(),
    }
    .serve(wire, entry)
}

fn entry_points<'tcx>(tcx: TyCtxt<'tcx>) -> io::Result<[MonoItem<'tcx>; 2]> {
    let (entry, _) = tcx
        .entry_fn(())
        .filter(|(id, _)| id.is_local())
        .ok_or_else(|| io::Error::other("a local Rust main is required"))?;
    let main = Instance::mono(tcx, entry);
    let env = TypingEnv::fully_monomorphized();
    let result = tcx
        .fn_sig(main.def_id())
        .no_bound_vars()
        .unwrap()
        .output()
        .no_bound_vars()
        .unwrap();
    let result = tcx.normalize_erasing_regions(env, Unnormalized::new_wip(result));
    let start = tcx
        .lang_items()
        .start_fn()
        .ok_or_else(|| io::Error::other("Rust lang_start is missing"))?;
    let start = Instance::expect_resolve(tcx, env, start, tcx.mk_args(&[result.into()]), DUMMY_SP);
    Ok([MonoItem::Fn(main), MonoItem::Fn(start)])
}

struct Guest(Child);

impl Drop for Guest {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

struct Producer<'a, 'tcx> {
    tcx: TyCtxt<'tcx>,
    backend: &'a LlvmCodegenBackend,
    items: HashMap<Symbol, (MonoItem<'tcx>, bool)>,
    cache: Option<&'a mut Cache>,
    report: bool,
}

impl<'tcx> Producer<'_, 'tcx> {
    fn serve(&mut self, wire: &mut UnixStream, entry: [MonoItem<'tcx>; 2]) -> io::Result<()> {
        let mut response = BufWriter::new(wire.try_clone()?);
        let mut request = Vec::new();
        loop {
            let mut bytes = [0; 4];
            if wire.read(&mut bytes[..1])? == 0 {
                return Ok(());
            }
            wire.read_exact(&mut bytes[1..])?;
            let len = u32::from_le_bytes(bytes) as usize;
            if len == 0 || len > 1024 * 1024 {
                return Err(io::Error::other("invalid symbol request"));
            }
            request.resize(len, 0);
            wire.read_exact(&mut request)?;
            let request = std::str::from_utf8(&request).map_err(io::Error::other)?;
            let visit = request.starts_with('!');
            let name = request.strip_prefix('!').unwrap_or(request);
            if name == "main" {
                self.emit(&mut response, name, &entry, visit)?;
            } else {
                let item = self
                    .items
                    .get(&Symbol::intern(name))
                    .map(|&(item, _)| item)
                    .ok_or_else(|| io::Error::other(format!("unknown requested symbol {name}")))?;
                self.emit(&mut response, name, &[item], visit)?;
            }
        }
    }

    fn emit(
        &mut self,
        wire: &mut impl Write,
        name: &str,
        items: &[MonoItem<'tcx>],
        visit: bool,
    ) -> io::Result<()> {
        let tcx = self.tcx;
        let mut request = CodegenUnit::new(Symbol::intern(name));
        request.set_symbol_name(Symbol::intern(name));
        let mut definitions = Vec::new();
        for &item in items {
            let signature = Symbol::intern(item.symbol_name(tcx).name);
            let state = self.items.entry(signature).or_insert((item, false));
            if state.0 != item || state.1 {
                return Err(io::Error::other(format!(
                    "conflicting IR request for {signature}"
                )));
            }
            state.1 = true;
            request.items_mut().insert(
                item,
                MonoItemData {
                    inlined: false,
                    linkage: Linkage::External,
                    visibility: Visibility::Default,
                    size_estimate: 0,
                },
            );
            if let MonoItem::Fn(instance) = item {
                let (used, _) = tcx
                    .items_of_instance((instance, CollectionMode::UsedItems))
                    .map_err(|_| io::Error::other(format!("cannot monomorphize {signature}")))?;
                for used in used {
                    let item = used.node;
                    if matches!(item, MonoItem::GlobalAsm(_)) {
                        return Err(io::Error::other("global assembly is unsupported"));
                    }
                    let name = Symbol::intern(item.symbol_name(tcx).name);
                    match self.items.entry(name) {
                        std::collections::hash_map::Entry::Vacant(slot) => {
                            slot.insert((item, false));
                        }
                        std::collections::hash_map::Entry::Occupied(slot)
                            if slot.get().0 != item =>
                        {
                            return Err(io::Error::other(format!(
                                "conflicting definitions for {name}"
                            )));
                        }
                        _ => {}
                    }
                    if !items.contains(&item) {
                        definitions.push((name, matches!(item, MonoItem::Fn(_))));
                    }
                }
            } else if let MonoItem::Static(id) = item {
                // Native TLS definitions are rejected by the dependency
                // image loader anyway.
                if tcx.is_thread_local_static(id) {
                    return Err(io::Error::other(
                        "thread-local statics are unsupported in demand IR",
                    ));
                }
                let alloc = tcx
                    .eval_static_initializer(id)
                    .map_err(|_| io::Error::other(format!("cannot evaluate static {signature}")))?;
                // A static's relocations need the same on-demand
                // materialization a function's used items get: record each
                // provenance target as a known item so the JIT can request
                // it when the static's code is linked.
                for (_offset, tag) in alloc.inner().provenance().ptrs().iter() {
                    let used = match tcx.global_alloc(tag.alloc_id()) {
                        rustc_middle::mir::interpret::GlobalAlloc::Function { instance } => {
                            MonoItem::Fn(instance)
                        }
                        rustc_middle::mir::interpret::GlobalAlloc::Static(def) => {
                            MonoItem::Static(def)
                        }
                        _ => continue,
                    };
                    let name = Symbol::intern(used.symbol_name(tcx).name);
                    match self.items.entry(name) {
                        std::collections::hash_map::Entry::Vacant(slot) => {
                            slot.insert((used, false));
                        }
                        std::collections::hash_map::Entry::Occupied(slot)
                            if slot.get().0 != used =>
                        {
                            return Err(io::Error::other(format!(
                                "conflicting definitions for {name}"
                            )));
                        }
                        _ => {}
                    }
                    if !items.contains(&used) {
                        definitions.push((name, matches!(used, MonoItem::Fn(_))));
                    }
                }
            }
        }
        // `codegen_unit` is synthetic. Its key must bind its members before
        // red/green validation, including both roots in the entry unit.
        if self.cache.is_some() {
            definitions.sort_unstable_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
            definitions.dedup();
            let identity = tcx.with_stable_hashing_context(|mut context| {
                let mut hash = StableHasher::new();
                request.stable_hash(&mut context, &mut hash);
                definitions.stable_hash(&mut context, &mut hash);
                hash.finish::<Fingerprint>()
            });
            request.set_name(Symbol::intern(&format!("ns-{identity}")));
        }
        let unit = tcx.arena.alloc(request);
        ACTIVE_UNIT.set((unit as *const CodegenUnit<'_>).cast());
        let guard = UnitGuard;
        if let Some(cache) = &mut self.cache {
            // Establish the codegen-unit query before rustc follows prior
            // edges. Its Symbol key cannot be reconstructed by the query engine.
            let _ = tcx.codegen_unit(unit.name());
            if let Some(record) = cache.reuse(tcx, unit, name, visit) {
                if visit {
                    return Ok(());
                }
                if self.report {
                    eprintln!("naspeed demand IR reused: {name}");
                }
                let payload = &record[32..];
                send_count(wire, payload.len())?;
                wire.write_all(payload)?;
                return wire.flush();
            }
        }
        if visit {
            return Ok(());
        }
        if self.report {
            for item in items {
                eprintln!("naspeed demand IR: {}", item.symbol_name(tcx).name);
            }
        }
        let (module, _) = self.backend.compile_codegen_unit(tcx, unit.name());
        drop(guard);
        tcx.dcx().abort_if_errors();
        let ir = LlvmCodegenBackend::serialize_module(module.module_llvm, false);
        let mut dependencies = Vec::new();
        send_count(&mut dependencies, definitions.len())?;
        for (name, callable) in definitions {
            dependencies.push(u8::from(callable));
            send_count(&mut dependencies, name.as_str().len())?;
            dependencies.extend_from_slice(name.as_str().as_bytes());
        }
        let parts = [&dependencies[..], ir.data()];
        if let Some(cache) = &mut self.cache {
            cache.store(tcx, unit.name(), name, &parts)?;
        }
        send_count(wire, parts.iter().map(|p| p.len()).sum())?;
        for part in parts {
            wire.write_all(part)?;
        }
        wire.flush()
    }
}

fn send_count(wire: &mut impl Write, len: usize) -> io::Result<()> {
    let len = u32::try_from(len).map_err(io::Error::other)?;
    wire.write_all(&len.to_le_bytes())
}
