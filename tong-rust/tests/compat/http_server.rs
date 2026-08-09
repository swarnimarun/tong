//! A minimal static HTTP server for the differential suite's registry
//! fixture: cargo's sparse index client needs a real HTTP endpoint (its
//! `file://` transports are unreliable), and tests must stay hermetic.
//! Serves files from a root directory with correct Content-Length and
//! Content-Type; 404 for missing files.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::thread;

/// Starts a static file server on 127.0.0.1 serving `root`; returns the
/// base URL (e.g. `http://127.0.0.1:49321/`).
pub fn serve(root: PathBuf) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
    let port = listener.local_addr().unwrap().port();
    let base = format!("http://127.0.0.1:{port}/");
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let root = root.clone();
            thread::spawn(move || handle(stream, &root));
        }
    });
    base
}

fn handle(mut stream: TcpStream, root: &Path) {
    let mut request = [0u8; 8192];
    let read = stream.read(&mut request).unwrap_or(0);
    let request = String::from_utf8_lossy(&request[..read]);
    let Some(request_line) = request.lines().next() else {
        return;
    };
    let path = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .trim_start_matches('/');
    // Percent-decode enough for our fixture paths.
    let path = path.replace("%20", " ").replace("%2F", "/");
    let file = root.join(&path);
    let response = if file.is_file() {
        match std::fs::read(&file) {
            Ok(body) => {
                let content_type = if path.ends_with(".json") {
                    "application/json"
                } else {
                    "application/octet-stream"
                };
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .into_bytes()
                    .into_iter()
                    .chain(body)
                    .collect()
            }
            Err(_) => b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
        }
    } else {
        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
    };
    let _ = stream.write_all(&response);
}

/// Serves a registry index (a temp dir with `config.json` + sparse index
/// files) and returns (base_url, server_guard). The guard keeps the server
/// alive for the test's duration.
pub fn serve_registry(index_dir: &Path) -> String {
    serve(index_dir.to_path_buf())
}

/// `[source.crates-io]` replacement config pointing at a served registry.
pub fn cargo_source_config(base_url: &str) -> String {
    format!(
        "[source.crates-io]\nreplace-with = \"local\"\n\n\
         [source.local]\nregistry = \"sparse+{base_url}\"\n"
    )
}
