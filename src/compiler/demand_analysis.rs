//! Keep rustc's own per-body query results; never serialize compiler ids ourselves.
use rustc_hir::def_id::LOCAL_CRATE;
use rustc_interface::interface::Compiler;
use rustc_middle::dep_graph::{DepKind, DepNode, WorkProductMap};
use rustc_middle::ty::TyCtxt;

pub(super) fn run_incremental<E>(
    compiler: &Compiler,
    krate: &mut rustc_ast::Crate,
    action: impl for<'tcx> FnOnce(TyCtxt<'tcx>) -> Result<WorkProductMap, E>,
) -> Result<(), E> {
    // Own the session so rustc can serialize and drop its query graph before
    // we finalize it, without entering whole-crate codegen or the linker.
    let krate = rustc_ast::Crate {
        attrs: std::mem::take(&mut krate.attrs),
        items: std::mem::take(&mut krate.items),
        ..*krate
    };
    let (result, session) = rustc_interface::create_and_enter_global_ctxt(compiler, krate, |tcx| {
        let _ = tcx.resolver_for_lowering();
        let products = action(tcx)?;
        let hash = tcx.crate_hash(LOCAL_CRATE);
        tcx.dep_graph.with_ignore(|| {
            rustc_incremental::save_work_product_index(
                tcx.sess,
                tcx.incr_comp_session,
                &tcx.dep_graph,
                products,
            );
        });
        tcx.dcx().abort_if_errors();
        Ok(hash)
    });
    rustc_incremental::finalize_session_directory(&compiler.sess, session, Some(result?));
    Ok(())
}

pub(super) fn check(tcx: TyCtxt<'_>, phase: &str) {
    tcx.ensure_ok().analysis(());
    tcx.dcx().abort_if_errors();
    if tcx.sess.opts.unstable_opts.query_dep_graph && report() {
        for id in tcx.hir_body_owners() {
            let node = DepNode::construct(tcx, DepKind::typeck_root, &id);
            if tcx.dep_graph.debug_was_loaded_from_disk(node) {
                eprintln!(
                    "naspeed demand typeck disk hit ({phase}): {}",
                    tcx.def_path_str(id)
                );
            }
        }
    }
}

pub(super) fn report() -> bool {
    std::env::var_os("NASPEED_LAZY_RUN_REPORT").as_deref() == Some(std::ffi::OsStr::new("1"))
}
