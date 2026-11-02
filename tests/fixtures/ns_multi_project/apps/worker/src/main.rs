use std::io::{self, Write};
use std::thread;
use std::time::Duration;

fn main() {
    let pid = std::process::id();
    let mark = std::env::var("WORKER_MARK").unwrap_or_default();
    loop {
        println!("worker pid {pid} {mark} {}", shared::value());
        let _ = io::stdout().flush();
        thread::sleep(Duration::from_millis(200));
    }
}
