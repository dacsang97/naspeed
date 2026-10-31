use serde::Serialize;

#[derive(Serialize)]
pub struct Greeting {
    pub text: String,
    pub count: u32,
}

pub fn table_len() -> usize {
    include_bytes!(concat!(env!("OUT_DIR"), "/table.bin")).len()
}

#[cfg(feature = "loud")]
pub fn shout(s: &str) -> String {
    s.to_uppercase()
}
