fn main() {
    let g = greeting::Greeting {
        text: "hello".into(),
        count: 3,
    };
    let mut buf = itoa::Buffer::new();
    let lz4 = unsafe { lz4_sys::LZ4_versionNumber() };
    println!("{} {} lz4={}", serde_json::to_string(&g).unwrap(), buf.format(42), lz4 > 0);
}
