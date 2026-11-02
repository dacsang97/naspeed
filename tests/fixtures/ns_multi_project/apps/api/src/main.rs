use std::io::{Read, Write};
use std::net::TcpListener;

fn main() {
    let pid = std::process::id();
    let mark = std::env::var("API_MARK").unwrap_or_default();
    let listener = TcpListener::bind(("127.0.0.1", 18081)).expect("bind fixture API");
    println!("\x1b[31mapi pid {pid}\x1b[0m");
    for mut stream in listener.incoming().flatten() {
        let mut request = [0; 1024];
        let _ = stream.read(&mut request);
        let body = format!(
            "api pid {pid} {mark} {} {}",
            shared::value(),
            api_only::value()
        );
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = stream.flush();
    }
}
