//! Standalone Cargo workspace wrapper, built against the pinned rustc-dev ABI.
//! rustc still performs analysis, computes the incremental dependency graph,
//! and emits, caches, and links the objects. The split-link alias can also
//! route dependencies through a cached Rust dylib on eligible dev runs.
#![feature(rustc_private)]

extern crate rustc_ast;
extern crate rustc_codegen_llvm;
extern crate rustc_codegen_ssa;
extern crate rustc_data_structures;
extern crate rustc_driver;
extern crate rustc_hir;
extern crate rustc_incremental;
extern crate rustc_interface;
extern crate rustc_middle;
extern crate rustc_session;
extern crate rustc_span;
extern crate rustc_symbol_mangling;
extern crate sha2;
extern crate rustc_attr_ir;
extern crate rustc_crate_store;

mod cargo_demand;
mod demand_analysis;
mod demand_cache;
mod demand_image;
mod demand_ir;
mod lazy_run;
mod split_link;

use std::ffi::OsStr;
use std::hash::{BuildHasher, BuildHasherDefault, DefaultHasher};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, ExitCode};
use std::sync::OnceLock;

use rustc_data_structures::either::Either;
use rustc_hir::attrs::Linkage;
use rustc_hir::def_id::LOCAL_CRATE;
use rustc_interface::interface::Config;
use rustc_middle::middle::codegen_fn_attrs::CodegenFnAttrFlags;
use rustc_middle::mono::{
    CodegenUnit, CodegenUnitNameBuilder, CollectionMode, InstantiationMode, MonoItem, MonoItemData,
    MonoItemPartitions, Visibility,
};
use rustc_middle::ty::TyCtxt;
use rustc_middle::util::Providers;
use rustc_session::Session;
use rustc_session::config::{Lto, OptLevel, host_tuple};
use rustc_span::Symbol;

const NATIVE_RUSTC: &str = env!("NASPEED_DRIVER_RUSTC");
const SYSROOT: &str = env!("NASPEED_DRIVER_SYSROOT");
const PARTITION_SCHEMA: &str = "naspeed-fn-v2";
const SHARD_TARGET: usize = 16;
const MAX_SHARDS: usize = 64;
const LARGE_BODY: usize = 256;
type Partitioner = for<'tcx> fn(TyCtxt<'tcx>, ()) -> MonoItemPartitions<'tcx>;
static NATIVE_PARTITIONER: OnceLock<Partitioner> = OnceLock::new();

struct Driver<'a> {
    partition: bool,
    report: bool,
    split_link: bool,
    lazy_run: bool,
    patch_libs: bool,
    artifact: Option<lazy_run::RunArtifact>,
    demand_argv: Option<&'a [String]>,
}

impl rustc_driver::Callbacks for Driver<'_> {
    fn config(&mut self, config: &mut Config) {
        if self.lazy_run {
            self.artifact = lazy_run::RunArtifact::configure(config, self.demand_argv.is_some())
                .unwrap_or_else(|error| {
                    rustc_session::EarlyDiagCtxt::new(config.opts.error_format)
                        .early_fatal(format!("naspeed lazy run: {error}"))
                });
        } else if self.patch_libs {
            lazy_run::RunArtifact::configure_lib_bitcode(config)
                .unwrap_or_else(|error| {
                    rustc_session::EarlyDiagCtxt::new(config.opts.error_format)
                        .early_fatal(format!("naspeed lazy run: {error}"))
                });
        }
        if self.split_link {
            split_link::configure(config);
        }
        if self.partition
            && config.opts.incremental.is_some()
            && config.opts.optimize == OptLevel::No
        {
            config.override_queries = Some(install_partition_queries);
            if self.report {
                config.opts.unstable_opts.print_mono_items = true;
                config.opts.unstable_opts.query_dep_graph = true;
            }
        }
    }

    fn after_crate_root_parsing(
        &mut self,
        compiler: &rustc_interface::interface::Compiler,
        krate: &mut rustc_ast::Crate,
    ) -> rustc_driver::Compilation {
        let (Some(artifact), Some(argv)) = (&mut self.artifact, self.demand_argv) else {
            return rustc_driver::Compilation::Continue;
        };
        if artifact.shares_app_text() {
            return rustc_driver::Compilation::Continue;
        }
        let result: std::io::Result<()> =
            demand_analysis::run_incremental(compiler, krate, |tcx| {
                rustc_interface::passes::write_dep_info(tcx);
                rustc_interface::passes::write_interface(tcx);
                demand_analysis::check(tcx, "cargo");
                artifact.prepare(tcx, Some(argv))?;
                Ok(Default::default())
            });
        if let Err(error) = result {
            compiler
                .sess
                .dcx()
                .fatal(format!("naspeed lazy run: {error}"));
        }
        rustc_driver::Compilation::Stop
    }

    fn after_analysis<'tcx>(
        &mut self,
        _: &rustc_interface::interface::Compiler,
        tcx: TyCtxt<'tcx>,
    ) -> rustc_driver::Compilation {
        if let Some(artifact) = &mut self.artifact {
            tcx.dcx().abort_if_errors();
            if let Err(error) = artifact.prepare(tcx, self.demand_argv) {
                tcx.dcx().fatal(format!("naspeed lazy run: {error}"));
            }
        }
        rustc_driver::Compilation::Continue
    }
}

fn install_partition_queries(_: &Session, providers: &mut Providers) {
    let _ = NATIVE_PARTITIONER.set(providers.queries.collect_and_partition_mono_items);
    providers.queries.collect_and_partition_mono_items = partition;
    // The upstream provider linearly scans the partitions. With one unit per
    // function, use their sorted names to avoid a quadratic compile step.
    providers.queries.codegen_unit = |tcx, name| {
        let units = tcx.collect_and_partition_mono_items(()).codegen_units;
        let index = units
            .binary_search_by(|unit| unit.name().as_str().cmp(name.as_str()))
            .expect("rustc requested an unknown codegen unit");
        &units[index]
    };
}

fn partition<'tcx>(tcx: TyCtxt<'tcx>, key: ()) -> MonoItemPartitions<'tcx> {
    let native = NATIVE_PARTITIONER.get().unwrap()(tcx, key);
    let sess = tcx.sess;
    if sess.opts.target_triple.tuple() != host_tuple()
        || sess.lto() != Lto::No
        || sess.opts.cg.codegen_units == Some(1)
        || sess.instrument_coverage()
        || sess.link_dead_code()
        || sess.opts.cg.linker_plugin_lto.enabled()
        || sess.opts.cg.profile_generate.enabled()
        || sess.opts.cg.profile_use.is_some()
        || !sess.opts.cg.llvm_args.is_empty()
        || !sess.opts.unstable_opts.sanitizer.is_empty()
        || sess.opts.unstable_opts.codegen_backend.is_some()
        || native.codegen_units.iter().any(|unit| {
            unit.items()
                .keys()
                .any(|item| matches!(item, MonoItem::GlobalAsm(_)))
        })
    {
        return native;
    }

    let mut names = CodegenUnitNameBuilder::new(tcx);
    let mut units = Vec::with_capacity(native.codegen_units.len());
    let mut changed = false;
    for base in native.codegen_units {
        if split_unit(tcx, &mut names, base, &mut units) {
            changed = true;
        } else {
            let mut keep = new_unit(base.name(), base.symbol_name(), base.is_primary());
            keep.items_mut().clone_from(base.items());
            finish_unit(&mut units, keep);
        }
    }
    if !changed {
        return native;
    }
    units.sort_unstable_by(|a, b| a.name().as_str().cmp(b.name().as_str()));
    MonoItemPartitions {
        all_mono_items: native.all_mono_items,
        codegen_units: tcx.arena.alloc_from_iter(units),
    }
}

fn split_unit<'tcx>(
    tcx: TyCtxt<'tcx>,
    names: &mut CodegenUnitNameBuilder<'tcx>,
    base: &CodegenUnit<'tcx>,
    units: &mut Vec<CodegenUnit<'tcx>>,
) -> bool {
    let is_root =
        |item: &MonoItem<'_>, data: &MonoItemData| matches!(item, MonoItem::Fn(_)) && !data.inlined;
    let mut roots = 0usize;
    let mut has_inline = false;
    let mut has_static = false;
    for (item, data) in base.items() {
        if data.inlined {
            has_inline = true;
        } else {
            if data.linkage == Linkage::Internal && item.explicit_linkage(tcx).is_some() {
                return false;
            }
            if let MonoItem::Static(def_id) = *item {
                // A retained registration or custom section can depend on
                // archive-member placement, not just on symbol visibility.
                let attrs = tcx.codegen_fn_attrs(def_id);
                if attrs.link_section.is_some()
                    || attrs.flags.intersects(
                        CodegenFnAttrFlags::USED_COMPILER | CodegenFnAttrFlags::USED_LINKER,
                    )
                {
                    return false;
                }
                has_static = true;
            }
            roots += usize::from(is_root(item, data));
        }
    }
    if roots < 2 {
        return false;
    }

    // Keep small groups and expensive bodies at function granularity. For
    // groups of cheap bodies, bound LLVM modules and object-file fan-out.
    // Hash stable symbol names so an unrelated insertion does not shift all
    // later functions. The bucket count changes only at power-of-two bands.
    let shard_count = if roots <= SHARD_TARGET {
        0
    } else {
        roots
            .div_ceil(SHARD_TARGET)
            .next_power_of_two()
            .min(MAX_SHARDS)
    };
    let mut shards: Vec<Option<CodegenUnit<'tcx>>> = (0..shard_count).map(|_| None).collect();
    let mut children = Vec::new();
    let mut anchor = new_unit(base.name(), base.symbol_name(), base.is_primary());
    let keep_inline_anchor = has_static || base.is_primary();
    let report = tcx.sess.opts.unstable_opts.print_mono_items;
    let hash = BuildHasherDefault::<DefaultHasher>::default();
    let mut pending = Vec::new();
    for (&item, &data) in base.items() {
        if !is_root(&item, &data) {
            if !data.inlined || keep_inline_anchor {
                let data = if data.inlined {
                    data
                } else {
                    split_linkage(data)
                };
                anchor.items_mut().insert(item, data);
            }
            continue;
        }
        let symbol = item.symbol_name(tcx);
        let unit = if shard_count == 0 || data.size_estimate >= LARGE_BODY {
            let identity = CodegenUnit::mangle_name(symbol.name);
            let name =
                names.build_cgu_name(LOCAL_CRATE, [PARTITION_SCHEMA, &identity], None::<&str>);
            children.push(named_unit(tcx, name));
            children.last_mut().unwrap()
        } else {
            let i = hash.hash_one(symbol.name) as usize & (shard_count - 1);
            shards[i].get_or_insert_with(|| {
                let name = names.build_cgu_name(
                    LOCAL_CRATE,
                    [PARTITION_SCHEMA, base.name().as_str()],
                    Some(i),
                );
                named_unit(tcx, name)
            })
        };
        unit.items_mut().insert(item, split_linkage(data));
        if has_inline && !attach_inline(tcx, base, unit, item, &mut pending) {
            return false;
        }
    }
    for unit in children.into_iter().chain(shards.into_iter().flatten()) {
        if report {
            let name = unit.name();
            for (item, data) in unit.items() {
                if !data.inlined {
                    println!("NASPEED_FUNCTION {item} {name}");
                }
            }
        }
        finish_unit(units, unit);
    }
    if anchor.is_primary() || !anchor.items().is_empty() {
        finish_unit(units, anchor);
    }
    true
}

fn attach_inline<'tcx>(
    tcx: TyCtxt<'tcx>,
    base: &CodegenUnit<'tcx>,
    unit: &mut CodegenUnit<'tcx>,
    root: MonoItem<'tcx>,
    pending: &mut Vec<MonoItem<'tcx>>,
) -> bool {
    pending.push(root);
    while let Some(MonoItem::Fn(instance)) = pending.pop() {
        let Ok((used, _)) = tcx.items_of_instance((instance, CollectionMode::UsedItems)) else {
            return false;
        };
        for used in used {
            let item = used.node;
            match base.items().get(&item) {
                Some(data) if data.inlined => {
                    if unit.items_mut().insert(item, *data).is_none() {
                        pending.push(item);
                    }
                }
                None if item.instantiation_mode(tcx) == InstantiationMode::LocalCopy => {
                    return false;
                }
                _ => {}
            }
        }
    }
    true
}

fn new_unit<'tcx>(name: Symbol, symbol: Symbol, primary: bool) -> CodegenUnit<'tcx> {
    let mut unit = CodegenUnit::new(name);
    unit.set_symbol_name(symbol);
    if primary {
        unit.make_primary();
    }
    unit
}

fn named_unit<'tcx>(tcx: TyCtxt<'tcx>, name: Symbol) -> CodegenUnit<'tcx> {
    let symbol = Symbol::intern(&rustc_symbol_mangling::mangle_cgu(
        tcx,
        LOCAL_CRATE,
        Either::Right(name.as_str()),
    ));
    new_unit(name, symbol, false)
}

fn finish_unit<'tcx>(units: &mut Vec<CodegenUnit<'tcx>>, mut unit: CodegenUnit<'tcx>) {
    unit.compute_size_estimate();
    units.push(unit);
}

fn split_linkage(mut data: MonoItemData) -> MonoItemData {
    // A private symbol formerly shared within one object must resolve from
    // sibling objects, but must not become interposable outside the DSO.
    if data.linkage == Linkage::Internal {
        data.linkage = Linkage::External;
        data.visibility = Visibility::Hidden;
    }
    data
}

fn main() -> ExitCode {
    let mut args = std::env::args_os();
    let executable = args.next().unwrap_or_default();
    let wrapper = Path::new(&executable).file_name();
    let split_wrapper = wrapper == Some(OsStr::new("naspeed-rustc-split"));
    let lazy_wrapper = wrapper == Some(OsStr::new("naspeed-rustc-lazy"));
    let demand_wrapper = wrapper == Some(OsStr::new("naspeed-rustc-demand"));
    let Some(original) = args.next() else {
        eprintln!("naspeed function driver: expected Cargo's rustc argument");
        return ExitCode::FAILURE;
    };
    if lazy_wrapper && original == "--naspeed-demand-ir" {
        return demand_ir::run(args);
    }
    if demand_wrapper && original == "--naspeed-demand-recipe" {
        return demand_ir::run_recipe(args.next());
    }
    let args: Vec<_> = args.collect();
    let mut partition = false;
    let mut probe = false;
    for arg in &args {
        if let Some(s) = arg.to_str() {
            probe |= s == "-vV" || s.starts_with("--print=");
            partition |= s
                .strip_prefix("--emit=")
                .is_some_and(|kinds| kinds.split(',').any(|kind| kind == "link"));
        }
    }
    let primary = std::env::var_os("CARGO_PRIMARY_PACKAGE").is_some();
    let bin = std::env::var_os("CARGO_BIN_NAME").is_some();
    let lazy_run = (lazy_wrapper || demand_wrapper) && primary && bin && partition && !probe;
    // A workspace library under `ns build --lazy-ir` isn't a lazy-run
    // candidate itself, but its bodies are patchable: emit per-CGU
    // bitcode so the executable's patch manifest can see them.
    let patch_libs = demand_wrapper
        && primary
        && !bin
        && partition
        && !probe
        && demand_image::patch_selectors().is_some();
    let native_args = args.iter().any(|arg| {
        arg.to_str()
            .is_none_or(|s| s.starts_with('@') || s == "--sysroot" || s == "--sysroot=")
    });
    if lazy_run && (Path::new(&original) != Path::new(NATIVE_RUSTC) || native_args) {
        eprintln!(
            "naspeed lazy run: set RUSTC to the pinned rustc executable; custom sysroots and response files are unsupported"
        );
        return ExitCode::FAILURE;
    }
    // The ABI-matched compiler library is already mapped. Answer Cargo's
    // probes here instead of exec'ing a second driver that maps it again.
    // Preserve native dispatch for unfamiliar argv and explicit sysroots.
    if Path::new(&original) != Path::new(NATIVE_RUSTC) || !(probe || partition) || native_args {
        let err = Command::new(original).args(args).exec();
        eprintln!("naspeed function driver: {err}");
        return ExitCode::FAILURE;
    }
    let mut argv = Vec::with_capacity(args.len() + 3);
    argv.push(original.into_string().unwrap());
    argv.extend(args.into_iter().map(|arg| arg.into_string().unwrap()));
    argv.extend(["--sysroot".into(), SYSROOT.into()]);
    if lazy_run && !split_link::candidate(&argv[1..argv.len() - 2]) {
        eprintln!("naspeed lazy run: custom linker and codegen flags are unsupported");
        return ExitCode::FAILURE;
    }
    let split_link = split_wrapper
        && partition
        && !probe
        && primary
        && bin
        && split_link::candidate(&argv[1..argv.len() - 2]);
    let mut driver = Driver {
        // Split linking is independent of function sharding. Keep native
        // CGUs in this mode: sharding regressed real Cargo run workloads.
        partition: partition && !probe && !split_wrapper && !lazy_wrapper && !demand_wrapper,
        split_link,
        lazy_run,
        patch_libs,
        artifact: None,
        demand_argv: (demand_wrapper && lazy_run).then_some(argv.as_slice()),
        report: std::env::var_os("NASPEED_FUNCTION_CODEGEN_REPORT").as_deref()
            == Some(OsStr::new("1")),
    };
    let early_dcx =
        rustc_session::EarlyDiagCtxt::new(rustc_session::config::ErrorOutputType::default());
    rustc_driver::init_rustc_env_logger(&early_dcx);
    rustc_driver::install_ctrlc_handler();
    rustc_driver::install_ice_hook("https://github.com/dacsang97/naspeed/issues", |_| ());
    rustc_driver::catch_with_exit_code(|| {
        rustc_driver::run_compiler(&argv, &mut driver);
        if let Some(artifact) = &mut driver.artifact {
            if let Err(error) = artifact.publish() {
                eprintln!("naspeed lazy run: {error}");
                return ExitCode::FAILURE;
            }
        }
        ExitCode::SUCCESS
    })
}
