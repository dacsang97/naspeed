//! Minimal HTTP server: `hello-server [--port N]` (default 8080, or `PORT`).
//! Answers every request with `greeter::greeting()` and `GREETING_SUFFIX`.

use std::io::{Read, Write};
use std::net::TcpListener;

fn main() {
    let mut port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8080);
    let args: Vec<String> = std::env::args().collect();
    if let Some(i) = args.iter().position(|a| a == "--port") {
        port = args
            .get(i + 1)
            .and_then(|p| p.parse().ok())
            .expect("--port <N>");
    }
    if args.iter().any(|a| a == "--version") {
        // NS_VERSION is set by `ns release`; plain cargo builds fall back.
        let version = option_env!("NS_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"));
        println!("hello-server {version}");
        return;
    }
    let listener = TcpListener::bind(("127.0.0.1", port)).expect("bind");
    println!("listening on 127.0.0.1:{port}");
    let suffix = std::env::var("GREETING_SUFFIX").unwrap_or_default();
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        let mut buf = [0u8; 1024];
        let _ = stream.read(&mut buf);
        let mut body = format!("{}{}\n", greeter::greeting(), suffix);
        if cfg!(feature = "shout") {
            body = body.to_uppercase();
        }
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        );
    }
}
