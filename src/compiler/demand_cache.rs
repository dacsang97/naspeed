//! Store pre-LLVM-optimization IR as a rustc incremental work product.
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use rustc_incremental::in_incr_comp_dir_sess;
use rustc_middle::dep_graph::{WorkProduct, WorkProductMap};
use rustc_middle::mono::CodegenUnit;
use rustc_middle::ty::TyCtxt;
use rustc_span::Symbol;
use sha2::{Digest, Sha256};

const RECORD: &str = "demand-ir";
const MAX_PAYLOAD: usize = 128 * 1024 * 1024;

pub struct Cache {
    generation: PathBuf,
    products: WorkProductMap,
}

impl Cache {
    pub fn new(generation: PathBuf) -> io::Result<Self> {
        fs::create_dir_all(&generation)?;
        Ok(Self {
            generation,
            products: WorkProductMap::default(),
        })
    }

    pub fn into_work_products(self) -> WorkProductMap {
        self.products
    }

    pub fn reuse<'tcx>(
        &mut self,
        tcx: TyCtxt<'tcx>,
        unit: &CodegenUnit<'tcx>,
        name: &str,
        visit: bool,
    ) -> Option<Vec<u8>> {
        let id = unit.work_product_id();
        let product = tcx.dep_graph.previous_work_product(&id)?;
        let file = product.saved_files.get(RECORD)?;
        let path = in_incr_comp_dir_sess(tcx.incr_comp_session.unwrap(), file);
        let mut record = Vec::new();
        File::open(&path)
            .ok()?
            .take((32 + MAX_PAYLOAD + 1) as u64)
            .read_to_end(&mut record)
            .ok()?;
        if record.len() <= 32
            || record.len() > 32 + MAX_PAYLOAD
            || record[..32] != digest(name, &[&record[32..]])[..]
        {
            return None;
        }
        tcx.dep_graph
            .try_mark_green(tcx, &unit.codegen_dep_node(tcx))?;
        self.products.insert(id, product);
        if !visit {
            self.publish(&path, name);
        }
        Some(record)
    }

    pub fn store(
        &mut self,
        tcx: TyCtxt<'_>,
        cgu: Symbol,
        name: &str,
        parts: &[&[u8]],
    ) -> io::Result<()> {
        let cgu_name = cgu.as_str();
        let file = format!("{cgu_name}.{RECORD}");
        let path = in_incr_comp_dir_sess(tcx.incr_comp_session.unwrap(), &file);
        let stage = path.with_extension("new");
        let mut out = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&stage)?;
        out.write_all(&digest(name, parts))?;
        for part in parts {
            out.write_all(part)?;
        }
        // rustc hardlinks previous work products into this session. Never
        // truncate one, or an older running launcher could observe new bytes.
        fs::rename(&stage, &path)?;
        self.publish(&path, name);
        let mut saved_files = rustc_data_structures::unord::UnordMap::default();
        saved_files.insert(RECORD.to_owned(), file);
        self.products.insert(
            rustc_middle::dep_graph::WorkProductId::from_cgu_name(cgu_name),
            WorkProduct {
                cgu_name: cgu_name.to_owned(),
                saved_files,
            },
        );
        Ok(())
    }

    fn publish(&self, path: &Path, name: &str) {
        let destination = self.generation.join(format!("{:x}", Sha256::digest(name)));
        match fs::hard_link(path, &destination) {
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            _ => return,
        }
        let stage = path.with_extension("publish");
        // Replace a corrupt record atomically without writing through its
        // inode. Concurrent workers never share the staging directory.
        if fs::hard_link(path, &stage).is_ok() {
            let _ = fs::rename(&stage, destination);
            let _ = fs::remove_file(&stage);
        }
    }
}

fn digest(name: &str, parts: &[&[u8]]) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(name);
    hash.update([0]);
    for part in parts {
        hash.update(part);
    }
    hash.finalize().into()
}
