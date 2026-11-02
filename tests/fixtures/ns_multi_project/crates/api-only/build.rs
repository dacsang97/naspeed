fn main() {
    println!("cargo:rerun-if-changed=src/lib.rs");
    println!("cargo:rerun-if-env-changed=NS_DEV_TEST_BUILD_DELAY_MS");
    if let Ok(delay) = std::env::var("NS_DEV_TEST_BUILD_DELAY_MS")
        && let Ok(delay) = delay.parse::<u64>()
    {
        std::thread::sleep(std::time::Duration::from_millis(delay));
    }
}
