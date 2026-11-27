//! Cargo still builds dependencies. The selected executable produces bitcode
//! and an executable launcher entry, never an AOT application binary.
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::num::NonZeroUsize;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rustc_hir::def_id::LOCAL_CRATE;
use rustc_interface::interface::Config;
use rustc_middle::ty::TyCtxt;
use rustc_session::config::{
    CrateType, DebugInfo, OptLevel, OutFileName, OutputType, OutputTypes, host_tuple,
};
use rustc_span::Symbol;
use rustc_session::search_paths::PathKind;

use rustc_attr_ir::NativeLibKind;
use rustc_crate_store::NativeLib;

use super::demand_image::NativeInput;

use super::SYSROOT;

#[path = "patch_gc.rs"]
mod patch_gc;

const APP_CACHE_KEEP: &str = "NASPEED_LAZY_APP_CACHE_KEEP";

pub struct RunArtifact {
    generation: PathBuf,
    launcher: PathBuf,
    save: bool,
    executable: Option<PathBuf>,
    demand: bool,
    app_image: bool,
    app_patch: Option<String>,
    app_cache_keep: Option<NonZeroUsize>,
    /// Dependency directory and crate stem for locating this crate's
    /// `*-cgu.*.rcgu.bc` emissions.
    bitcode_dir: PathBuf,
    crate_stem: String,
    /// The crate's root source file (rustc's `opts.input` path), handed
    /// to patch preparation so the source compiler reads the same target.
    source_file: Option<PathBuf>,
    natives: usize,
    app_link: Option<(super::demand_image::NativeLink, Vec<PathBuf>, Vec<NativeInput>)>,
}

impl RunArtifact {
    /// A workspace library under `ns build --lazy-ir` keeps its normal
    /// Cargo outputs and additionally emits per-CGU bitcode next to the
    /// objects the archive packs. The final executable's patch manifest
    /// globs every `.rcgu.bc` whose sibling `.o` exists, so a lib body is
    /// patchable like a bin body. Stale units from an earlier emit are
    /// removed before codegen so only this build's pair survives.
    pub fn configure_lib_bitcode(config: &mut Config) -> Result<(), String> {
        let opts = &mut config.opts;
        if !cfg!(all(target_os = "macos", target_arch = "aarch64"))
            || opts.target_triple.tuple() != host_tuple()
            || opts.optimize != OptLevel::No
            || opts.test
        {
            return Ok(());
        }
        let Some(out) = config.output_dir.as_ref() else {
            return Err("lazy run requires an output directory".into());
        };
        let stem = format!(
            "{}{}.",
            opts.crate_name.as_deref().unwrap_or_default().replace('-', "_"),
            opts.cg.extra_filename.as_str()
        );
        for entry in fs::read_dir(out).into_iter().flatten().flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with(&stem) && name.ends_with(".rcgu.bc") {
                let _ = fs::remove_file(entry.path());
            }
        }
        let mut outputs: Vec<(OutputType, Option<OutFileName>)> =
            opts.output_types.iter().map(|(k, v)| (*k, v.clone())).collect();
        outputs.push((OutputType::Bitcode, None));
        opts.output_types = OutputTypes::new(&outputs);
        opts.cg.codegen_units = Some(16);
        Ok(())
    }
}

impl RunArtifact {
    pub fn configure(config: &mut Config, demand: bool) -> Result<Option<Self>, String> {
        let opts = &mut config.opts;
        // Cargo also invokes rustc for build scripts and libraries. Their
        // artifacts retain their original formats and semantics.
        if opts.crate_types != [CrateType::Executable] || !opts.output_types.should_link() {
            return Ok(None);
        }
        if !cfg!(all(target_os = "macos", target_arch = "aarch64"))
            || opts.target_triple.tuple() != host_tuple()
            || opts.optimize != OptLevel::No
            || opts.test
            || !opts.libs.is_empty()
            || config.output_file.is_some()
            || opts.output_types.len() != 2
            || !opts.output_types.contains_key(&OutputType::DepInfo)
        {
            return Err("lazy run requires an unoptimized Darwin arm64 Cargo executable without custom linking".into());
        }
        let Some(out) = config.output_dir.as_ref() else {
            return Err("Cargo did not supply an output directory".into());
        };
        let mut artifact = Self::create(out, demand, opts.crate_name.as_deref(), Some(opts.cg.extra_filename.as_str())).map_err(|err| err.to_string())?;
        artifact.source_file = config.input.opt_path().map(|p| p.to_path_buf());
        // Per-CGU bitcode + objects land in the dependency directory under
        // rustc's own names; publish() moves the bitcode inputs into the
        // generation while rustc's incremental cache reuses unchanged
        // objects.
        let extra: Vec<(OutputType, Option<OutFileName>)> = if artifact.app_patch.is_some() {
            vec![(OutputType::Bitcode, None), (OutputType::Object, None)]
        } else if artifact.app_image {
            vec![(
                OutputType::Object,
                Some(OutFileName::Real(artifact.app_object())),
            )]
        } else if demand {
            vec![(OutputType::Metadata, None)]
        } else {
            vec![(
                OutputType::Bitcode,
                Some(OutFileName::Real(artifact.bitcode())),
            )]
        };
        let mut outputs = vec![(OutputType::DepInfo, None)];
        outputs.extend(extra);
        opts.output_types = OutputTypes::new(&outputs);
        opts.cg.prefer_dynamic = true;
        if artifact.app_patch.is_some() {
            // Per-CGU bitcode gives patch preparation incremental inputs:
            // an edit rewrites only the codegen units it touched.
            opts.cg.codegen_units = Some(16);
            opts.debuginfo = DebugInfo::None;
        } else {
            opts.cg.codegen_units = Some(1);
            opts.cli_forced_codegen_units = Some(1);
        }
        if demand {
            // Checking must not replace the worker's graph, whose codegen
            // nodes prove reuse of IR that this process never requests.
            let cache = if artifact.app_patch.is_some() {
                "app-patch-v1"
            } else if artifact.app_image {
                "app-native-v1"
            } else {
                "analysis-v1"
            };
            opts.incremental = Some(artifact.generation.parent().unwrap().join(cache));
            opts.unstable_opts.query_dep_graph |= super::demand_analysis::report();
        }
        // Per-CGU emissions of previous builds are dead input: remove them
        // up front so publish() globs only this build's units.
        for path in artifact
            .cgu_files(".rcgu.bc")
            .into_iter()
            .chain(artifact.cgu_files(".rcgu.o"))
            .flatten()
        {
            let _ = fs::remove_file(path);
        }
        let files = [
            artifact.payload(),
            artifact.manifest(),
            artifact.generation.clone(),
            artifact.launcher.clone(),
        ];
        let driver = std::env::current_exe().map_err(|err| err.to_string())?;
        // Rustc writes the dep-info before codegen. Generated members will
        // have fixed mtimes so Cargo doesn't mark them newer than this build.
        config.track_state = Some(Box::new(move |sess| {
            let mut tracked = sess.file_depinfo.borrow_mut();
            if let Some(path) = driver.to_str() {
                tracked.insert(Symbol::intern(path));
            }
            for path in files {
                tracked.insert(Symbol::intern(path.to_str().unwrap()));
            }
            if demand {
                super::demand_image::track(sess);
            }
            sess.env_depinfo.borrow_mut().insert((
                Symbol::intern(APP_CACHE_KEEP),
                std::env::var(APP_CACHE_KEEP)
                    .ok()
                    .as_deref()
                    .map(Symbol::intern),
            ));
        }));
        Ok(Some(artifact))
    }

    fn create(out: &Path, demand: bool, crate_name: Option<&str>, extra_filename: Option<&str>) -> io::Result<Self> {
        let launcher = std::env::current_exe()?.with_file_name("naspeed-lazy-launch");
        if !launcher.is_file() {
            return Err(io::Error::other(format!(
                "missing {}; run make lazy-driver",
                launcher.display()
            )));
        }
        let cache = out.canonicalize()?.join(".naspeed-lazy");
        if cache.to_str().is_none() || launcher.to_str().is_none() {
            return Err(io::Error::other(
                "lazy run requires UTF-8 tool and target paths",
            ));
        }
        let app_patch = demand.then(super::demand_image::patch_selectors).flatten();
        // Patch generations and their dylib images accumulate into
        // gigabytes on real workspaces, so collection defaults on: keep a
        // handful of recent generations (running processes pin theirs via
        // the lease either way) and reclaim anything unreferenced.
        // NASPEED_LAZY_APP_CACHE_KEEP=N keeps more; "off" restores the old
        // collect-never behavior.
        let app_cache_keep = match (app_patch.is_some(), std::env::var(APP_CACHE_KEEP)) {
            (true, Ok(value)) if value != "off" => {
                Some(value.parse::<NonZeroUsize>().map_err(|_| {
                    io::Error::other(
                        "NASPEED_LAZY_APP_CACHE_KEEP must be a positive integer or off",
                    )
                })?)
            }
            (true, Err(std::env::VarError::NotUnicode(_))) => {
                return Err(io::Error::other(
                    "NASPEED_LAZY_APP_CACHE_KEEP must be UTF-8",
                ));
            }
            (true, _) => NonZeroUsize::new(8),
            _ => None,
        };
        fs::create_dir_all(&cache)?;
        let tick = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_nanos();
        let generation = cache.join(format!("{}-{tick:x}", std::process::id()));
        fs::create_dir(&generation)?;
        Ok(Self {
            generation,
            launcher,
            source_file: None,
            save: false,
            executable: None,
            demand,
            app_image: demand && super::demand_image::app_enabled(),
            app_patch,
            app_cache_keep,
            app_link: None,
            natives: 0,
            bitcode_dir: out.to_path_buf(),
            crate_stem: format!(
                "{}{}",
                crate_name.unwrap_or_default().replace('-', "_"),
                extra_filename.unwrap_or_default()
            ),
        })
    }

    fn bitcode(&self) -> PathBuf {
        self.generation.join("app.bc")
    }

    /// Every CGU bitcode in the dependency directory whose sibling object
    /// exists: the executable's own units plus any workspace library units
    /// emitted under `NASPEED_LAZY_APP_PATCH`. A lib object stays paired
    /// with its bitcode for rustc's incremental reuse, so the pair is the
    /// freshness signal — stale or half-published files are skipped.
    fn bitcodes(&self) -> io::Result<Vec<PathBuf>> {
        let mut files = Vec::new();
        for entry in fs::read_dir(&self.bitcode_dir)? {
            let path = entry?.path();
            let is_bc = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".rcgu.bc"));
            if !is_bc {
                continue;
            }
            let object = path.with_extension("o");
            if object.is_file() {
                files.push(path);
            }
        }
        files.sort();
        Ok(files)
    }

    /// This crate's `*.{stem}.*.rcgu.<ext>` emissions in the dependency
    /// directory, ordered by name.
    fn cgu_files(&self, extension: &str) -> io::Result<Vec<PathBuf>> {
        let prefix = format!("{}.", self.crate_stem);
        let mut files = Vec::new();
        for entry in fs::read_dir(&self.bitcode_dir)? {
            let path = entry?.path();
            let name = match path.file_name().and_then(|name| name.to_str()) {
                Some(name) => name,
                None => continue,
            };
            if name.starts_with(&prefix) && name.ends_with(extension) {
                files.push(path);
            }
        }
        files.sort();
        Ok(files)
    }


    /// Newline-separated CGU bitcode paths consumed by
    /// `--app-patch-prepare`.
    fn bitcode_manifest(&self) -> PathBuf {
        self.generation.join("app-bcs")
    }

    pub fn shares_app_text(&self) -> bool {
        self.app_image || self.app_patch.is_some()
    }

    fn app_object(&self) -> PathBuf {
        self.generation.join("app.o")
    }

    fn payload(&self) -> PathBuf {
        if self.demand {
            self.generation.join("recipe")
        } else {
            self.bitcode()
        }
    }

    fn manifest(&self) -> PathBuf {
        self.generation.join("manifest")
    }

    fn live_feed(&self) -> PathBuf {
        self.generation.join("live-feed")
    }

    fn stage_live_update(&self, executable: &Path) -> io::Result<(PathBuf, PathBuf)> {
        let name = executable
            .file_name()
            .ok_or_else(|| io::Error::other("Cargo executable path does not have a file name"))?;
        let live = self.generation.parent().unwrap().join("live");
        fs::create_dir_all(&live)?;
        let update = live.join(name);
        if !update.is_absolute() {
            return Err(io::Error::other(
                "lazy app patch live update path is not absolute",
            ));
        }
        let feed = self.live_feed();
        fs::write(&feed, update.as_os_str().as_encoded_bytes())?;
        File::open(&feed)?.set_modified(UNIX_EPOCH)?;

        let generation = self.generation.file_name().ok_or_else(|| {
            io::Error::other("lazy app patch generation does not have a file name")
        })?;
        let stage = live.join(format!(".pending-{}", generation.to_string_lossy()));
        let plan = self.generation.join("patch-plan");
        let result = if plan.is_file() {
            fs::hard_link(plan, &stage)
        } else {
            File::options()
                .write(true)
                .create_new(true)
                .open(&stage)
                .and_then(|file| file.set_modified(UNIX_EPOCH))
        };
        if let Err(error) = result {
            let _ = fs::remove_file(&stage);
            return Err(error);
        }
        Ok((stage, update))
    }

    pub fn prepare(&mut self, tcx: TyCtxt<'_>, demand_argv: Option<&[String]>) -> io::Result<()> {
        let sysroot = Path::new(SYSROOT);
        if tcx.has_global_allocator(LOCAL_CRATE) {
            return Err(io::Error::other(
                "custom global allocators are not supported in lazy run",
            ));
        }
        // Shared libraries and frameworks resolve only through a linked
        // dependency image; admit them by enabling it when natives demand.
        let needs_image = |krate| {
            tcx.native_libraries(krate)
                .iter()
                .any(|lib| matches!(lib.kind, NativeLibKind::Framework { .. }))
        };
        let image_enabled = self.demand
            && !self.app_image
            && self.app_patch.is_none()
            && (super::demand_image::enabled()
                || needs_image(LOCAL_CRATE)
                || tcx.crates(()).iter().copied().any(needs_image));
        let mut locals = Vec::new();
        for lib in tcx.native_libraries(LOCAL_CRATE) {
            locals.push(self.resolve_native(tcx, lib, image_enabled)?);
        }
        let mut std = None;
        let mut archives = Vec::new();
        let mut sources = Vec::new();
        let mut natives = Vec::new();
        for &krate in tcx.crates(()) {
            let src = tcx.used_crate_source(krate);
            if tcx.crate_dep_kind(krate).macros_only() {
                continue;
            }
            if tcx.crate_name(krate).as_str() == "std" {
                std = src
                    .dylib
                    .as_deref()
                    .filter(|path| path.starts_with(sysroot))
                    .map(Path::to_path_buf)
                    .or_else(|| {
                        src.paths()
                            .next()
                            .filter(|path| path.starts_with(sysroot))
                            .map(|path| path.with_extension("dylib"))
                    });
            }
            let rlib = src.rlib.clone().or_else(|| {
                src.rmeta
                    .as_ref()
                    .map(|p| p.with_extension("rlib"))
                    .filter(|p| p.is_file())
            });
            let Some(path) = rlib else {
                return Err(io::Error::other(format!(
                    "lazy run requires a Rust archive for {}",
                    tcx.crate_name(krate)
                )));
            };
            if path.starts_with(sysroot) {
                continue;
            }
            if tcx.has_global_allocator(krate) {
                return Err(io::Error::other(format!(
                    "{} has a custom global allocator, unsupported in lazy run",
                    tcx.crate_name(krate)
                )));
            }
            for lib in tcx.native_libraries(krate) {
                // `#[link]` statics bundle their objects into the crate's
                // own .rlib by default; only unbundled ones need the file.
                if let NativeLibKind::Static {
                    bundle: Some(false),
                    ..
                } = lib.kind
                {
                } else if matches!(lib.kind, NativeLibKind::Static { .. }) {
                    continue;
                }
                natives.push(self.resolve_native(tcx, lib, image_enabled)?);
            }
            // Rustc publishes each .rlib by rename. Keep that exact inode
            // alive for a launcher even if Cargo rebuilds a dependency later.
            let name = format!("{}.rlib", archives.len());
            let snapshot = self.generation.join(name);
            fs::hard_link(&path, &snapshot)?;
            archives.push(snapshot);
            if image_enabled {
                sources.push(path);
            }
        }
        if let Some(argv) = demand_argv {
            super::demand_ir::validate(tcx)?;
            super::cargo_demand::write(&self.payload(), tcx, argv)?;
        }
        let std = std.filter(|p| p.is_file()).ok_or_else(|| {
            io::Error::other("lazy run requires the toolchain's dynamic Rust std")
        })?;
        let image = if !sources.is_empty() {
            match super::demand_image::prepare(
                tcx,
                self.generation.parent().unwrap(),
                &self.generation,
                &std,
                &sources,
                &archives,
                &natives,
            ) {
                Ok(image) => Some(image),
                Err(error) => {
                    if super::demand_analysis::report() {
                        eprintln!("naspeed dependency image unavailable: {error}");
                    }
                    None
                }
            }
        } else {
            None
        };
        let mut manifest = File::create(self.manifest())?;
        manifest.write_all(if self.app_image || self.app_patch.is_some() {
            b"naspeed-app-image-v1\0"
        } else if image.is_some() {
            b"naspeed-demand-image-v1\0"
        } else if self.demand {
            b"naspeed-demand-v1\0"
        } else {
            b"naspeed-orc-v1\0"
        })?;
        for path in std::iter::once(self.payload().as_path())
            .chain(std::iter::once(std.as_path()))
            .chain(image.as_deref())
            .chain(archives.iter().map(PathBuf::as_path))
            .chain(
                locals
                    .iter()
                    .chain(natives.iter())
                    .filter_map(|input| match input {
                        // Without an image, archives link into the guest and
                        // dylibs are dlopen'ed by the launcher.
                        NativeInput::Archive(path) | NativeInput::Dylib(path)
                            if image.is_none() =>
                        {
                            Some(path.as_path())
                        }
                        _ => None,
                    }),
            )
        {
            manifest.write_all(path.as_os_str().as_encoded_bytes())?;
            manifest.write_all(&[0])?;
        }
        manifest.set_modified(UNIX_EPOCH)?;
        let name = tcx.crate_name(LOCAL_CRATE);
        let OutFileName::Real(executable) = rustc_session::output::filename_for_input(
            tcx.sess,
            CrateType::Executable,
            name,
            tcx.output_filenames(()),
        ) else {
            return Err(io::Error::other("Cargo executable path is not a file"));
        };
        if self.app_image || self.app_patch.is_some() {
            match super::demand_image::NativeLink::new(tcx, &std) {
                Ok(link) => {
                    let mut natives = natives;
                    natives.extend(locals);
                    self.app_link = Some((link, archives, natives));
                }
                Err(error) if self.app_patch.is_some() => {
                    eprintln!("naspeed app patch unavailable; using ORC: {error}");
                }
                Err(error) => eprintln!("naspeed app image unavailable; using ORC: {error}"),
            }
        }
        self.executable = Some(executable);
        Ok(())
    }

    pub fn publish(&mut self) -> io::Result<()> {
        let Some(executable) = &self.executable else {
            return Err(io::Error::other(
                "Rust compiler did not analyze the selected executable",
            ));
        };
        let cache = self.generation.parent().unwrap();
        // Reserve a sweep only if no other build or reader has the cache.
        // Otherwise publish under a shared lock and leave GC to a later build.
        let publishing = if self.app_patch.is_some() {
            Some(patch_gc::begin(cache, self.app_cache_keep.is_some())?)
        } else {
            None
        };
        if !self.demand {
            File::open(self.bitcode())?.set_modified(UNIX_EPOCH)?;
        }
        if let Some(selectors) = self.app_patch.as_deref() {
            if let Some((link, archives, natives)) = self.app_link.take() {
                let prepared = self.bitcodes().and_then(|bitcodes| {
                    let mut manifest = String::with_capacity(bitcodes.len() * 64);
                    for path in &bitcodes {
                        manifest.push_str(&path.to_string_lossy());
                        manifest.push('\n');
                    }
                    fs::write(self.bitcode_manifest(), manifest)
                });
                let outcome = prepared.and_then(|()| {
                    link.application_patches(
                        &self.launcher,
                        &self.bitcode_manifest(),
                        &self.generation,
                        cache,
                        selectors,
                        &archives,
                        &natives,
                        self.source_file.as_deref(),
                    )
                });
                if let Err(error) = outcome {
                    eprintln!("naspeed app patch unavailable; using ORC: {error}");
                }
            }
            // Per-CGU inputs are only needed for this publish; leaving
            // them in deps/ would leak ~100 MB per build per crate.
            if std::env::var_os("NASPEED_LAZY_DUMP_PATCH").is_none()
                && std::env::var_os("NASPEED_LAZY_KEEP_WORK").is_none()
            {
                for path in self
                    .cgu_files(".rcgu.bc")
                    .into_iter()
                    .chain(self.cgu_files(".rcgu.o"))
                    .flatten()
                {
                    fs::remove_file(path)?;
                }
            }
            fs::remove_file(self.bitcode_manifest())?;
        } else if self.app_image {
            let object = self.app_object();
            if let Some((link, archives, natives)) = self.app_link.take() {
                match link.application(&object, &archives, &natives, &self.generation) {
                    Ok(()) if super::demand_analysis::report() => {
                        eprintln!("naspeed app image built: {}", self.generation.display());
                    }
                    Ok(()) => {}
                    Err(error) => eprintln!("naspeed app image unavailable; using ORC: {error}"),
                }
            }
            fs::remove_file(object)?;
        }
        let live_update = if self.app_patch.is_some() {
            Some(self.stage_live_update(executable)?)
        } else {
            None
        };
        let manifest = self.manifest();
        let result = (|| {
            let stage = self.generation.join("launch");
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o755)
                .open(&stage)?;
            writeln!(
                file,
                "#!/bin/sh\nexec {} {} \"$0\" \"$@\"",
                quote(self.launcher.as_os_str()),
                quote(manifest.as_os_str())
            )?;
            drop(file);
            if publishing.is_some() {
                let mut lease = File::options()
                    .write(true)
                    .create_new(true)
                    .open(self.generation.join("patch-lease"))?;
                lease.write_all(executable.file_name().unwrap().as_encoded_bytes())?;
                lease.set_modified(UNIX_EPOCH)?;
            }
            fs::rename(stage, executable)?;
            // Keep the completed generation if publishing its optional live
            // update fails; the launcher remains a valid cold-start fallback.
            self.save = true;
            if let Some((stage, update)) = &live_update {
                fs::rename(stage, update)?;
            }
            // Reset last: every write above (archives, lease, live update)
            // bumps the generation directory mtime, which cargo dep-info
            // compares on the next build — leaving it current marks the
            // crate dirty and forces a spurious rebuild.
            File::open(&self.generation)?.set_modified(UNIX_EPOCH)?;
            Ok(())
        })();
        if result.is_err()
            && let Some((stage, _)) = &live_update
        {
            let _ = fs::remove_file(stage);
        }
        if result.is_ok()
            && publishing.as_ref().is_some_and(|(_, exclusive)| *exclusive)
            && let Some(keep) = self.app_cache_keep
            && let Err(error) = patch_gc::sweep(
                cache,
                &self.generation,
                executable.file_name().unwrap(),
                keep,
            )
        {
            eprintln!("naspeed app cache GC skipped: {error}");
        }
        result
    }

    /// Resolve one `#[link]` declaration to an input the launcher images
    /// understand. Static archives always work: without a dependency image
    /// they link into the ORC guest like the rlib snapshots, and with one
    /// they are force-loaded into it. Shared libraries and frameworks need
    /// an image path (ORC cannot pull symbols from a `.dylib` lazily), so
    /// outside it they still reject the lazy run.
    fn resolve_native(
        &mut self,
        tcx: TyCtxt<'_>,
        lib: &NativeLib,
        image: bool,
    ) -> io::Result<NativeInput> {
        let unsupported = |what: &str| {
            io::Error::other(format!(
                "{what} native libraries are not supported in lazy run"
            ))
        };
        if lib.cfg.is_some() || lib.verbatim.is_some() || lib.has_modifiers() {
            return Err(io::Error::other(format!(
                "conditional, verbatim or modified native libraries are not supported in lazy run ({}: cfg={:?} fm={:?} verbatim={:?} modifiers={:?} kind={:?})",
                lib.name, lib.cfg.is_some(), lib.foreign_module.is_some(),
                lib.verbatim.is_some(), lib.has_modifiers(), lib.kind,
            )));
        }
        let name = lib.name.as_str();
        let images = image || self.app_image || self.app_patch.is_some();
        match lib.kind {
            NativeLibKind::Static { .. } => {
                // `#[link]` semantics: whole-archive is the same objects the
                // image force-loads anyway.
                let Some(path) = self.find_native(tcx, &format!("lib{name}.a")) else {
                    return Err(io::Error::other(format!(
                        "lazy run cannot find static native library {name}"
                    )));
                };
                self.snapshot(path, ".a").map(NativeInput::Archive)
            }
            NativeLibKind::Dylib { .. } | NativeLibKind::Unspecified => {
                if let Some(path) = self.find_native(tcx, &format!("lib{name}.dylib")) {
                    // A Cargo-built dylib: keep it alive in the generation
                    // and let the launcher dlopen the snapshot.
                    return self.snapshot(path, ".dylib").map(NativeInput::Dylib);
                }
                if images {
                    // SDK and toolchain libraries resolve as `-l` in images.
                    return Ok(NativeInput::Shared(name.to_string()));
                }
                // Plain ORC can dlopen the real file: look it up in /usr/lib.
                let path = Path::new("/usr/lib").join(format!("lib{name}.dylib"));
                if path.is_file() {
                    return self.snapshot(path, ".dylib").map(NativeInput::Dylib);
                }
                Err(unsupported("shared"))
            }
            NativeLibKind::Framework { .. } => {
                if !images {
                    // ORC cannot dlopen a framework by name; images link it.
                    return Err(unsupported("framework"));
                }
                Ok(NativeInput::Framework(name.to_string()))
            }
            NativeLibKind::RawDylib { .. } => Err(unsupported("raw-dylib")),
            NativeLibKind::LinkArg => Err(unsupported("link-arg")),
            NativeLibKind::WasmImportModule => Err(unsupported("wasm")),
        }
    }

    /// Resolve `libfoo.{a,dylib}` through Cargo's `-L` search dirs.
    fn find_native(&self, tcx: TyCtxt<'_>, filename: &str) -> Option<PathBuf> {
        tcx.sess
            .opts
            .search_paths
            .iter()
            .filter(|p| {
                matches!(
                    p.kind,
                    PathKind::Native | PathKind::Dependency | PathKind::All
                )
            })
            .map(|p| p.dir.join(filename))
            .find(|p| p.is_file())
    }

    /// Keep one resolved native file alive for the launcher, the same way
    /// the rlib snapshots keep archives alive.
    fn snapshot(&mut self, path: PathBuf, ext: &str) -> io::Result<PathBuf> {
        let name = format!("native-{}{}", self.natives, ext);
        self.natives += 1;
        let snapshot = self.generation.join(name);
        fs::hard_link(&path, &snapshot)?;
        Ok(snapshot)
    }
}

fn quote(arg: &OsStr) -> String {
    format!("'{}'", arg.to_str().unwrap().replace('\'', "'\\''"))
}

impl Drop for RunArtifact {
    fn drop(&mut self) {
        if !self.save {
            let _ = fs::remove_dir_all(&self.generation);
        }
    }
}
