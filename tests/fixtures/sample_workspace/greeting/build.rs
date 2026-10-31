// Writes a deterministic 1 MiB table to OUT_DIR, identical in every target
// dir, so `naspeed gc --dedup` has something to share.
fn main() {
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let table: Vec<u8> = (0..1024 * 1024u32).map(|i| (i.wrapping_mul(2654435761) >> 24) as u8).collect();
    std::fs::write(out.join("table.bin"), table).unwrap();
    println!("cargo:rerun-if-changed=build.rs");
}
