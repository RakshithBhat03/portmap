//! Shared fixtures: throwaway data directories, a `Shared` wired to inspectable
//! channels, and a tiny scripted HTTP server for exercising probes.

use crate::scanner::ScanMsg;
use crate::store::Store;
use crate::{Config, Shared};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new() -> TempDir {
        static N: AtomicUsize = AtomicUsize::new(0);
        let p = std::env::temp_dir().join(format!(
            "portmap-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(p.join("thumbs")).unwrap();
        TempDir(p)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub struct Harness {
    pub shared: Arc<Shared>,
    pub scan_rx: Receiver<ScanMsg>,
    pub thumb_rx: Receiver<u16>,
    pub dir: TempDir,
}

pub fn harness(port: u16, browser: Option<PathBuf>) -> Harness {
    let dir = TempDir::new();
    let (scan_tx, scan_rx) = mpsc::channel();
    let (thumb_tx, thumb_rx) = mpsc::channel();
    let shared = Arc::new(Shared {
        store: Mutex::new(Store::load(dir.path())),
        cfg: Config {
            port,
            data_dir: dir.path().to_path_buf(),
            stale_secs: 600,
            browser,
        },
        scan_tx,
        thumb_tx,
    });
    Harness {
        shared,
        scan_rx,
        thumb_rx,
        dir,
    }
}

/// Serves raw responses on an ephemeral loopback port: `respond` gets the request
/// path and returns the exact bytes to write back. Returns the port and a hit counter.
pub fn http_server(respond: fn(&str) -> Vec<u8>) -> (u16, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            counter.fetch_add(1, Ordering::SeqCst);
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let mut buf = Vec::new();
            let mut chunk = [0u8; 1024];
            while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                match stream.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                }
            }
            let req = String::from_utf8_lossy(&buf);
            let path = req.split_whitespace().nth(1).unwrap_or("/").to_string();
            let _ = stream.write_all(&respond(&path));
        }
    });
    (port, hits)
}

/// A loopback port with nothing listening on it.
pub fn closed_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

pub fn html(status: &str, title: &str) -> Vec<u8> {
    let body = format!("<html><head><title>{title}</title></head><body></body></html>");
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}
