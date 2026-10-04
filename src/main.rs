mod launchd;
mod scan;
mod scanner;
mod server;
mod store;
mod thumbs;

use std::path::PathBuf;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

pub struct Config {
    pub port: u16,
    pub data_dir: PathBuf,
    pub stale_secs: u64,
    pub browser: Option<PathBuf>,
}

pub struct Shared {
    pub cfg: Config,
    pub store: Mutex<store::Store>,
    pub scan_tx: Sender<scanner::ScanMsg>,
    pub thumb_tx: Sender<u16>,
}

const USAGE: &str = "portmap — bookmarks for everything listening on localhost

USAGE:
  portmap [serve] [--port N]   run the UI (default http://localhost:7878)
  portmap install [--port N]   run at login via a launchd user agent (macOS)
  portmap uninstall            remove the launchd agent
  portmap open                 open the UI in your browser

ENV:
  PORTMAP_PORT            default port (7878)
  PORTMAP_HOME            data directory (~/.portmap)
  PORTMAP_STALE_MINUTES   how long stopped services linger before clearing (10)
  PORTMAP_BROWSER         Chromium-family binary used for thumbnails (auto-detected)
  PORTMAP_NO_THUMBS=1     disable thumbnails";

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut port: u16 = std::env::var("PORTMAP_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(7878);
    if let Some(i) = args.iter().position(|a| a == "--port" || a == "-p") {
        match args.get(i + 1).and_then(|p| p.parse().ok()) {
            Some(p) => port = p,
            None => fail("--port needs a number"),
        }
        args.drain(i..=i + 1);
    }
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()));
    let data_dir = std::env::var("PORTMAP_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home.join(".portmap"));

    let result = match args.first().map(String::as_str) {
        None | Some("serve") => serve(port, data_dir),
        Some("install") => {
            let _ = std::fs::create_dir_all(&data_dir);
            launchd::install(port, &data_dir)
        }
        Some("uninstall") => launchd::uninstall(),
        Some("open") => std::process::Command::new(if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        })
        .arg(format!("http://localhost:{port}"))
        .status()
        .map(|_| ())
        .map_err(|e| e.to_string()),
        Some("-h" | "--help" | "help") => {
            println!("{USAGE}");
            Ok(())
        }
        Some("-V" | "--version") => {
            println!("portmap {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some(other) => Err(format!("unknown command `{other}`\n\n{USAGE}")),
    };
    if let Err(e) = result {
        fail(&e);
    }
}

fn fail(msg: &str) -> ! {
    eprintln!("portmap: {msg}");
    std::process::exit(1);
}

fn serve(port: u16, data_dir: PathBuf) -> Result<(), String> {
    std::fs::create_dir_all(data_dir.join("thumbs"))
        .map_err(|e| format!("cannot create {}: {e}", data_dir.display()))?;
    let stale_minutes: u64 = std::env::var("PORTMAP_STALE_MINUTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10);
    let browser = thumbs::find_browser();
    match &browser {
        Some(b) => eprintln!("thumbnails via {}", b.display()),
        None => eprintln!("no Chromium-family browser found; thumbnails disabled"),
    }
    let (scan_tx, scan_rx) = mpsc::channel();
    let (thumb_tx, thumb_rx) = mpsc::channel();
    let shared = Arc::new(Shared {
        store: Mutex::new(store::Store::load(&data_dir)),
        cfg: Config {
            port,
            data_dir,
            stale_secs: stale_minutes * 60,
            browser,
        },
        scan_tx,
        thumb_tx,
    });
    // First scan before accepting requests so the page never opens empty.
    scanner::cycle(&shared, false);
    {
        let s = shared.clone();
        thread::Builder::new()
            .name("scanner".into())
            .spawn(move || scanner::run(s, scan_rx))
            .map_err(|e| e.to_string())?;
    }
    {
        let s = shared.clone();
        thread::Builder::new()
            .name("thumbs".into())
            .spawn(move || thumbs::run(s, thumb_rx))
            .map_err(|e| e.to_string())?;
    }
    server::run(shared)
}
