//! Tailscale awareness: this machine's tailnet name and its `tailscale serve` mappings, so a
//! card opened from another tailnet device links somewhere that device can actually reach.
//! portmap only reads Tailscale state, except when the user shares or unshares a port.

use serde_json::Value;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Tailnet {
    /// MagicDNS name of this machine, e.g. `box.tail1234.ts.net`.
    pub host: String,
    pub ips: Vec<String>,
    /// Login of the user who owns this node, as `tailscale serve` reports it in
    /// `Tailscale-User-Login`.
    pub owner: Option<String>,
    /// MagicDNS suffix of the tailnet, e.g. `tail1234.ts.net`. Every node's name ends in it.
    pub suffix: Option<String>,
    pub serves: Vec<Serve>,
}

/// One port this node serves on the tailnet.
#[derive(Debug, Clone, PartialEq)]
pub struct Serve {
    pub port: u16,
    pub https: bool,
    /// Loopback port the root handler proxies to; `None` for raw TCP or anything else.
    pub target: Option<u16>,
    /// Exactly what `share` creates (same port, a lone `/` handler), so `unshare` removes
    /// nothing else. Other mappings are linked to but left for the user to manage.
    pub owned: bool,
}

impl Tailnet {
    pub fn serve_for(&self, local_port: u16) -> Option<&Serve> {
        let matching = || {
            self.serves
                .iter()
                .filter(move |s| s.target == Some(local_port))
        };
        matching().find(|s| s.owned).or_else(|| matching().next())
    }

    pub fn serve_on(&self, port: u16) -> Option<&Serve> {
        self.serves.iter().find(|s| s.port == port)
    }

    /// Whether a listener bound to `addrs` already answers on the tailnet interface.
    pub fn reaches(&self, addrs: &[String]) -> bool {
        addrs.iter().any(|a| {
            matches!(a.as_str(), "*" | "0.0.0.0" | "[::]")
                || self
                    .ips
                    .iter()
                    .any(|ip| a.trim_matches(|c| c == '[' || c == ']') == ip)
        })
    }

    pub fn serve_url(&self, s: &Serve) -> String {
        let (scheme, default) = if s.https {
            ("https", 443)
        } else {
            ("http", 80)
        };
        if s.port == default {
            format!("{scheme}://{}/", self.host)
        } else {
            format!("{scheme}://{}:{}/", self.host, s.port)
        }
    }

    pub fn direct_url(&self, port: u16) -> String {
        format!("http://{}:{port}/", self.host)
    }
}

/// `PORTMAP_TAILSCALE` names the CLI or turns the integration `off`. Otherwise look on PATH
/// and in the usual install spots, since launchd agents get a minimal PATH.
pub fn find_cli() -> Option<PathBuf> {
    match std::env::var("PORTMAP_TAILSCALE") {
        Ok(v) if v == "off" || v == "0" => return None,
        Ok(v) if !v.is_empty() => return Some(PathBuf::from(v)).filter(|p| p.exists()),
        _ => {}
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|d| d.join("tailscale"))
        .chain(
            [
                "/usr/local/bin/tailscale",
                "/opt/homebrew/bin/tailscale",
                "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
            ]
            .map(PathBuf::from),
        )
        .find(|p| p.is_file())
}

/// Current tailnet identity and serve config: `Ok(None)` when Tailscale is installed but
/// not connected, `Err` when the CLI itself failed (worth riding out, see `set_tailnet`).
pub fn detect(cli: &Path) -> Result<Option<Tailnet>, String> {
    let status = run(cli, &["status", "--json"], Duration::from_secs(5))?;
    let Some(mut tailnet) = parse_status(&status) else {
        return Ok(None);
    };
    let serves = run(cli, &["serve", "status", "--json"], Duration::from_secs(5))?;
    tailnet.serves = parse_serve(&serves, &tailnet.host);
    Ok(Some(tailnet))
}

/// Tailnet-only HTTPS proxy from `<host>:<port>` to the same port on loopback. Never funnel.
pub fn share(cli: &Path, port: u16) -> Result<(), String> {
    run(
        cli,
        &[
            "serve",
            "--bg",
            &format!("--https={port}"),
            &format!("http://localhost:{port}"),
        ],
        Duration::from_secs(20),
    )
    .map(|_| ())
}

pub fn unshare(cli: &Path, s: &Serve) -> Result<(), String> {
    let flag = if s.https { "https" } else { "http" };
    run(
        cli,
        &["serve", &format!("--{flag}={}", s.port), "off"],
        Duration::from_secs(10),
    )
    .map(|_| ())
}

fn parse_status(json: &str) -> Option<Tailnet> {
    let v: Value = serde_json::from_str(json).ok()?;
    if v["BackendState"] != "Running" {
        return None;
    }
    let host = v["Self"]["DNSName"].as_str()?.trim_end_matches('.');
    if host.is_empty() {
        return None;
    }
    let ips = v["Self"]["TailscaleIPs"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|ip| ip.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let owner = v["Self"]["UserID"]
        .as_u64()
        .and_then(|id| v["User"][id.to_string()]["LoginName"].as_str())
        .map(str::to_string);
    let suffix = v["CurrentTailnet"]["MagicDNSSuffix"]
        .as_str()
        .or_else(|| v["MagicDNSSuffix"].as_str())
        .map(|s| s.trim_matches('.').to_string())
        .filter(|s| !s.is_empty());
    Some(Tailnet {
        host: host.to_string(),
        ips,
        owner,
        suffix,
        serves: Vec::new(),
    })
}

/// Reads `tailscale serve status --json`: `TCP` lists the ports, `Web` the HTTP handlers.
fn parse_serve(json: &str, host: &str) -> Vec<Serve> {
    let Ok(v) = serde_json::from_str::<Value>(json) else {
        return Vec::new();
    };
    let Some(tcp) = v["TCP"].as_object() else {
        return Vec::new();
    };
    let mut serves: Vec<Serve> = tcp
        .iter()
        .filter_map(|(port, cfg)| {
            let port: u16 = port.parse().ok()?;
            let https = cfg["HTTPS"] == true;
            let web = https || cfg["HTTP"] == true;
            let handlers = &v["Web"][format!("{host}:{port}")]["Handlers"];
            let target = web
                .then(|| handlers["/"]["Proxy"].as_str())
                .flatten()
                .and_then(loopback_port);
            let lone = handlers.as_object().is_some_and(|h| h.len() == 1);
            Some(Serve {
                port,
                https,
                target,
                owned: https && lone && target == Some(port),
            })
        })
        .collect();
    serves.sort_by_key(|s| s.port);
    serves
}

/// `http://127.0.0.1:3000`, `localhost:3000` or `3000` -> 3000; non-loopback targets -> None.
fn loopback_port(proxy: &str) -> Option<u16> {
    let rest = proxy
        .strip_prefix("http://")
        .or_else(|| proxy.strip_prefix("https+insecure://"))
        .or_else(|| proxy.strip_prefix("https://"))
        .unwrap_or(proxy);
    let authority = rest.split('/').next()?;
    let Some((host, port)) = authority.rsplit_once(':') else {
        return authority.parse().ok();
    };
    matches!(host, "127.0.0.1" | "localhost" | "[::1]")
        .then(|| port.parse().ok())
        .flatten()
}

/// Runs the CLI with a deadline: a wedged tailscaled must not stall scans or requests.
/// Errors carry the CLI's own message (e.g. "Serve is not enabled ... visit <url>").
fn run(cli: &Path, args: &[&str], timeout: Duration) -> Result<String, String> {
    let mut child = Command::new(cli)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run {}: {e}", cli.display()))?;
    let (tx, rx) = mpsc::channel();
    let pipes: [Option<Box<dyn Read + Send>>; 2] = [
        child.stdout.take().map(|p| Box::new(p) as _),
        child.stderr.take().map(|p| Box::new(p) as _),
    ];
    for (i, pipe) in pipes.into_iter().enumerate() {
        let tx = tx.clone();
        thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut p) = pipe {
                let _ = p.read_to_end(&mut buf);
            }
            let _ = tx.send((i, buf));
        });
    }
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    // The pipes close with the child; don't hang on a grandchild that inherited them.
    let mut out = [String::new(), String::new()];
    let until = Instant::now() + Duration::from_secs(1);
    for _ in 0..2 {
        match rx.recv_timeout(until.saturating_duration_since(Instant::now())) {
            Ok((i, buf)) => out[i] = String::from_utf8_lossy(&buf).into_owned(),
            Err(_) => break,
        }
    }
    let [stdout, stderr] = out;
    let said = summary(&stderr).or_else(|| summary(&stdout));
    match status {
        Some(s) if s.success() => Ok(stdout),
        Some(s) => Err(said.unwrap_or_else(|| format!("tailscale exited with {s}"))),
        None => Err(match said {
            Some(msg) => format!("tailscale did not finish: {msg}"),
            None => "tailscale did not respond".into(),
        }),
    }
}

/// Non-empty lines joined into one, short enough for a status line.
fn summary(text: &str) -> Option<String> {
    let joined: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let joined = joined.join(" ");
    (!joined.is_empty()).then(|| joined.chars().take(300).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOST: &str = "box.tail1234.ts.net";

    fn tailnet(serves: Vec<Serve>) -> Tailnet {
        Tailnet {
            host: HOST.into(),
            ips: vec!["100.64.0.7".into(), "fd7a:115c:a1e0::7".into()],
            owner: None,
            suffix: None,
            serves,
        }
    }

    fn addrs(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn status_needs_a_running_backend_and_a_name() {
        let up = r#"{"BackendState":"Running","Self":{"DNSName":"box.tail1234.ts.net.","TailscaleIPs":["100.64.0.7"],"UserID":42},"User":{"42":{"LoginName":"me@example.com"}},"CurrentTailnet":{"MagicDNSSuffix":"tail1234.ts.net"}}"#;
        let t = parse_status(up).unwrap();
        assert_eq!(t.host, HOST);
        assert_eq!(t.ips, ["100.64.0.7"]);
        assert_eq!(t.owner.as_deref(), Some("me@example.com"));
        assert_eq!(t.suffix.as_deref(), Some("tail1234.ts.net"));
        let stopped = up.replace("Running", "Stopped");
        assert!(parse_status(&stopped).is_none());
        assert!(parse_status(r#"{"BackendState":"Running","Self":{"DNSName":""}}"#).is_none());
        assert!(parse_status("not json").is_none());
    }

    #[test]
    fn serve_status_maps_ports_to_loopback_targets() {
        let json = r#"{
          "TCP": {"443": {"HTTPS": true}, "3000": {"HTTPS": true}, "8080": {"HTTP": true},
                  "5432": {"TCPForward": "127.0.0.1:5432"}, "9000": {"HTTPS": true}},
          "Web": {
            "box.tail1234.ts.net:443":  {"Handlers": {"/": {"Proxy": "http://127.0.0.1:7878"}}},
            "box.tail1234.ts.net:3000": {"Handlers": {"/": {"Proxy": "http://localhost:3000"}}},
            "box.tail1234.ts.net:8080": {"Handlers": {"/": {"Proxy": "http://127.0.0.1:5173/"}}},
            "box.tail1234.ts.net:9000": {"Handlers": {"/": {"Proxy": "http://10.0.0.5:9000"}}}
          }
        }"#;
        let s = parse_serve(json, HOST);
        let target = |port| s.iter().find(|x| x.port == port).unwrap().target;
        assert_eq!(s.len(), 5);
        assert_eq!(target(443), Some(7878));
        assert_eq!(target(3000), Some(3000));
        assert_eq!(target(8080), Some(5173));
        assert!(!s.iter().find(|x| x.port == 8080).unwrap().https);
        assert_eq!(target(5432), None, "raw TCP forwards are not web links");
        assert_eq!(target(9000), None, "non-loopback proxies are not ours");
        let owned = |port| s.iter().find(|x| x.port == port).unwrap().owned;
        assert!(owned(3000), "what `share` creates");
        assert!(!owned(443), "different local port");
        assert!(!owned(8080), "plain HTTP");
        let two_handlers = r#"{"TCP":{"3000":{"HTTPS":true}},"Web":{"box.tail1234.ts.net:3000":
            {"Handlers":{"/":{"Proxy":"http://127.0.0.1:3000"},"/api":{"Proxy":"http://127.0.0.1:4000"}}}}}"#;
        assert!(!parse_serve(two_handlers, HOST)[0].owned);
        assert!(parse_serve("{}", HOST).is_empty());
    }

    #[test]
    fn urls_omit_default_ports() {
        let t = tailnet(vec![]);
        let https = |port| Serve {
            port,
            https: true,
            target: None,
            owned: false,
        };
        assert_eq!(t.serve_url(&https(443)), "https://box.tail1234.ts.net/");
        assert_eq!(
            t.serve_url(&https(3000)),
            "https://box.tail1234.ts.net:3000/"
        );
        let http80 = Serve {
            port: 80,
            https: false,
            target: None,
            owned: false,
        };
        assert_eq!(t.serve_url(&http80), "http://box.tail1234.ts.net/");
        assert_eq!(t.direct_url(5000), "http://box.tail1234.ts.net:5000/");
    }

    #[test]
    fn only_wildcard_or_tailnet_binds_are_directly_reachable() {
        let t = tailnet(vec![]);
        assert!(t.reaches(&addrs(&["*"])));
        assert!(t.reaches(&addrs(&["127.0.0.1", "[::]"])));
        assert!(t.reaches(&addrs(&["100.64.0.7"])));
        assert!(t.reaches(&addrs(&["[fd7a:115c:a1e0::7]"])));
        assert!(!t.reaches(&addrs(&["127.0.0.1", "[::1]"])));
        assert!(!t.reaches(&addrs(&["192.168.1.4"])));
    }

    #[test]
    fn loopback_targets_parse() {
        assert_eq!(loopback_port("http://127.0.0.1:3000"), Some(3000));
        assert_eq!(loopback_port("http://[::1]:3000/app"), Some(3000));
        assert_eq!(loopback_port("https+insecure://localhost:8443"), Some(8443));
        assert_eq!(loopback_port("3000"), Some(3000));
        assert_eq!(loopback_port("http://example.com:3000"), None);
        assert_eq!(loopback_port("text:hello"), None);
    }

    #[test]
    fn cli_errors_and_timeouts_are_reported() {
        let sh = Path::new("/bin/sh");
        assert_eq!(
            run(sh, &["-c", "echo ok"], Duration::from_secs(5)).unwrap(),
            "ok\n"
        );
        assert_eq!(
            run(sh, &["-c", "echo boom >&2; exit 3"], Duration::from_secs(5)).unwrap_err(),
            "boom"
        );
        assert_eq!(
            run(sh, &["-c", "sleep 5"], Duration::from_millis(100)).unwrap_err(),
            "tailscale did not respond"
        );
        // What the CLI printed before hanging (e.g. a URL to enable Serve) is kept.
        assert_eq!(
            run(
                sh,
                &[
                    "-c",
                    "echo 'Serve is not enabled.' >&2; echo ' visit x' >&2; exec sleep 5"
                ],
                Duration::from_millis(300)
            )
            .unwrap_err(),
            "tailscale did not finish: Serve is not enabled. visit x"
        );
        // A grandchild holding the pipes open doesn't hold up the result.
        let t = Instant::now();
        assert!(run(sh, &["-c", "sleep 5 & echo done"], Duration::from_secs(5)).is_ok());
        assert!(t.elapsed() < Duration::from_secs(3));
    }
}
