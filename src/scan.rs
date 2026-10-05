//! Discovery primitives: who is listening, what process is it, and does it speak HTTP.
//! Everything here is stateless; caching and scheduling live in `scanner.rs`.

use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct Listener {
    pub port: u16,
    pub pid: u32,
    pub command: String,
    pub addrs: Vec<String>,
}

/// All TCP listeners visible to the current user, one per port.
pub fn listeners() -> Vec<Listener> {
    let Ok(out) = Command::new("lsof")
        .args(["-nP", "-iTCP", "-sTCP:LISTEN", "-Fpcn"])
        .output()
    else {
        return Vec::new();
    };
    parse_listeners(&String::from_utf8_lossy(&out.stdout))
}

/// Parses `lsof -Fpcn` output: `p`/`c` lines set the current process, `n` lines name
/// its sockets. Multiple sockets on one port (IPv4 + IPv6) collapse into one listener.
fn parse_listeners(text: &str) -> Vec<Listener> {
    let mut map: BTreeMap<u16, Listener> = BTreeMap::new();
    let (mut pid, mut cmd) = (0u32, String::new());
    for line in text.lines() {
        let Some(tag) = line.chars().next() else {
            continue;
        };
        let rest = &line[tag.len_utf8()..];
        match tag {
            'p' => pid = rest.parse().unwrap_or(0),
            'c' => cmd = rest.to_string(),
            'n' => {
                let Some(idx) = rest.rfind(':') else { continue };
                let Ok(port) = rest[idx + 1..].parse::<u16>() else {
                    continue;
                };
                let host = rest[..idx].to_string();
                let l = map.entry(port).or_insert_with(|| Listener {
                    port,
                    pid,
                    command: cmd.clone(),
                    addrs: Vec::new(),
                });
                if !l.addrs.contains(&host) {
                    l.addrs.push(host);
                }
            }
            _ => {}
        }
    }
    map.into_values().collect()
}

/// Full argv for each pid, in one `ps` call.
pub fn process_args(pids: &[u32]) -> HashMap<u32, String> {
    if pids.is_empty() {
        return HashMap::new();
    }
    let list = join_pids(pids);
    let Ok(out) = Command::new("ps")
        .args(["-o", "pid=,args=", "-p", &list])
        .output()
    else {
        return HashMap::new();
    };
    parse_ps_args(&String::from_utf8_lossy(&out.stdout))
}

fn parse_ps_args(text: &str) -> HashMap<u32, String> {
    let mut map = HashMap::new();
    for line in text.lines() {
        let line = line.trim_start();
        let Some((pid, args)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        if let Ok(pid) = pid.parse() {
            map.insert(pid, args.trim().to_string());
        }
    }
    map
}

/// Working directory for each pid, in one `lsof` call.
pub fn process_cwds(pids: &[u32]) -> HashMap<u32, PathBuf> {
    if pids.is_empty() {
        return HashMap::new();
    }
    let list = join_pids(pids);
    let Ok(out) = Command::new("lsof")
        .args(["-a", "-d", "cwd", "-Fn", "-p", &list])
        .output()
    else {
        return HashMap::new();
    };
    parse_cwds(&String::from_utf8_lossy(&out.stdout))
}

fn parse_cwds(text: &str) -> HashMap<u32, PathBuf> {
    let mut map = HashMap::new();
    let mut pid = 0u32;
    for line in text.lines() {
        if let Some(p) = line.strip_prefix('p') {
            pid = p.parse().unwrap_or(0);
        } else if let Some(n) = line.strip_prefix('n') {
            map.insert(pid, PathBuf::from(n));
        }
    }
    map
}

fn join_pids(pids: &[u32]) -> String {
    pids.iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

pub fn exe_path(args: &str) -> &str {
    // macOS app bundles contain spaces in their path; the executable ends at the first
    // " -" flag or at the end, which is good enough for classification.
    let first = args.split(" -").next().unwrap_or(args);
    if first.contains(".app/") {
        first.trim()
    } else {
        args.split_whitespace().next().unwrap_or("")
    }
}

pub fn exe_name(args: &str) -> String {
    let exe = exe_path(args);
    exe.rsplit('/').next().unwrap_or(exe).to_string()
}

/// OS daemons (AirPlay receiver, Handoff, ...) are never dev services.
pub fn is_system_process(args: &str) -> bool {
    let exe = exe_path(args);
    ["/System/", "/usr/libexec/", "/usr/sbin/", "/sbin/"]
        .iter()
        .any(|p| exe.starts_with(p))
}

const TOOLS: &[(&str, &str)] = &[
    ("next", "next"),
    ("nuxt", "nuxt"),
    ("nuxi", "nuxt"),
    ("astro", "astro"),
    ("remix", "remix"),
    ("svelte-kit", "sveltekit"),
    ("storybook", "storybook"),
    ("start-storybook", "storybook"),
    ("docusaurus", "docusaurus"),
    ("vite", "vite"),
    ("webpack-dev-server", "webpack"),
    ("webpack", "webpack"),
    ("parcel", "parcel"),
    ("wrangler", "wrangler"),
    ("expo", "expo"),
    ("metro", "metro"),
    ("jupyter-lab", "jupyter"),
    ("jupyter-notebook", "jupyter"),
    ("jupyter", "jupyter"),
    ("uvicorn", "uvicorn"),
    ("gunicorn", "gunicorn"),
    ("hypercorn", "hypercorn"),
    ("flask", "flask"),
    ("manage.py", "django"),
    ("streamlit", "streamlit"),
    ("http.server", "http.server"),
    ("rails", "rails"),
    ("puma", "puma"),
    ("hugo", "hugo"),
    ("jekyll", "jekyll"),
    ("ollama", "ollama"),
    ("nodemon", "nodemon"),
    ("tsx", "tsx"),
];

const CONTAINER_RUNTIMES: &[&str] = &[
    "OrbStack",
    "com.docker",
    "Docker.app",
    "docker-proxy",
    "podman",
    "Rancher Desktop",
    "colima",
    "limactl",
];

pub fn is_container_runtime(args: &str) -> bool {
    let exe = exe_path(args);
    CONTAINER_RUNTIMES.iter().any(|c| exe.contains(c))
}

/// Desktop apps (editors, launchers, chat clients) open ports for their own internals;
/// those are not things the user started. Container runtimes are the exception: they
/// publish ports on behalf of containers.
pub fn is_desktop_app(args: &str) -> bool {
    exe_path(args).contains(".app/") && !is_container_runtime(args)
}

/// A short human label for the runtime ("vite", "next", "python", ...).
pub fn runtime_label(args: &str, fallback: &str) -> String {
    if is_container_runtime(args) {
        return "container".into();
    }
    let tokens: Vec<&str> = args
        .split(|c: char| c.is_whitespace() || c == '/' || c == '\\')
        .map(|t| {
            t.trim_end_matches(".js")
                .trim_end_matches(".mjs")
                .trim_end_matches(".cjs")
                .trim_end_matches(".ts")
        })
        .collect();
    for (needle, label) in TOOLS {
        if tokens.iter().any(|t| t == needle) {
            return label.to_string();
        }
    }
    let name = exe_name(args);
    if name.is_empty() {
        fallback.to_string()
    } else {
        name.trim_end_matches(".exe")
            .trim_end_matches(|c: char| c.is_ascii_digit() || c == '.')
            .to_string()
    }
}

const MARKERS: &[&str] = &[
    "package.json",
    "Cargo.toml",
    "pyproject.toml",
    "go.mod",
    "Gemfile",
    "deno.json",
    "composer.json",
    "mix.exs",
    "pom.xml",
    "build.gradle",
    ".git",
];

/// Project name from a working directory: nearest project marker, prefixed with the
/// repository name when the project lives inside a larger repo (monorepo apps).
pub fn project_name(cwd: &Path, home: &Path) -> Option<String> {
    // Outside $HOME (e.g. /opt/homebrew/var/mysql) a marker almost never means "project".
    if home.as_os_str().is_empty() || !cwd.starts_with(home) {
        return None;
    }
    let stop = |d: &Path| d == home || d == Path::new("/");
    let nearest = cwd
        .ancestors()
        .take_while(|d| !stop(d))
        .find(|d| MARKERS.iter().any(|m| d.join(m).exists()))?;
    let repo = nearest
        .ancestors()
        .take_while(|d| !stop(d))
        .find(|d| d.join(".git").exists());
    let base = |p: &Path| p.file_name().map(|s| s.to_string_lossy().to_string());
    match repo {
        Some(r) if r != nearest => Some(format!("{}/{}", base(r)?, base(nearest)?)),
        _ => base(nearest),
    }
}

#[derive(Debug, Clone, Default)]
pub struct Probe {
    pub http: bool,
    pub status: u16,
    pub title: Option<String>,
    pub link_host: String,
}

fn candidate_ips(addrs: &[String]) -> Vec<IpAddr> {
    let mut ips: Vec<IpAddr> = Vec::new();
    for a in addrs {
        let ip = match a.as_str() {
            "*" | "0.0.0.0" | "127.0.0.1" => IpAddr::V4(Ipv4Addr::LOCALHOST),
            "[::]" | "[::1]" => IpAddr::V6(Ipv6Addr::LOCALHOST),
            other => match other.trim_matches(|c| c == '[' || c == ']').parse() {
                Ok(ip) => ip,
                Err(_) => continue,
            },
        };
        if !ips.contains(&ip) {
            ips.push(ip);
        }
    }
    if ips.is_empty() {
        ips.push(IpAddr::V4(Ipv4Addr::LOCALHOST));
    }
    ips
}

/// GET / on the port, following up to two same-port redirects to find a page title.
/// A listener that accepts TCP but never answers like HTTP is reported as `http: false`.
pub fn probe(port: u16, addrs: &[String]) -> Probe {
    for ip in candidate_ips(addrs) {
        let addr = SocketAddr::new(ip, port);
        let link_host = if ip.is_loopback() {
            "localhost".to_string()
        } else if ip.is_ipv6() {
            format!("[{ip}]")
        } else {
            ip.to_string()
        };
        let mut path = "/".to_string();
        for hop in 0..3 {
            match http_get(addr, port, &path) {
                Err(Refused) => break,
                Ok(None) => {
                    return Probe {
                        http: false,
                        link_host,
                        ..Default::default()
                    }
                }
                Ok(Some(resp)) => {
                    // The last hop's response is reported as-is, even if it redirects again.
                    if hop < 2 && (300..400).contains(&resp.status) {
                        if let Some(next) = resp
                            .location
                            .as_deref()
                            .and_then(|l| same_port_path(l, port))
                        {
                            if next != path {
                                path = next;
                                continue;
                            }
                        }
                    }
                    return Probe {
                        http: true,
                        status: resp.status,
                        title: extract_title(&resp.body),
                        link_host,
                    };
                }
            }
        }
    }
    Probe {
        http: false,
        link_host: "localhost".into(),
        ..Default::default()
    }
}

struct Refused;

struct Resp {
    status: u16,
    location: Option<String>,
    body: Vec<u8>,
}

fn http_get(addr: SocketAddr, port: u16, path: &str) -> Result<Option<Resp>, Refused> {
    let mut stream =
        TcpStream::connect_timeout(&addr, Duration::from_millis(500)).map_err(|_| Refused)?;
    let _ = stream.set_read_timeout(Some(Duration::from_millis(1500)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: localhost:{port}\r\nUser-Agent: portmap\r\nAccept: text/html,*/*\r\nAccept-Encoding: identity\r\nConnection: close\r\n\r\n"
    );
    if stream.write_all(req.as_bytes()).is_err() {
        return Ok(None);
    }
    let deadline = Instant::now() + Duration::from_millis(2500);
    let mut buf = Vec::with_capacity(16 * 1024);
    let mut chunk = [0u8; 8192];
    while buf.len() < 128 * 1024 && Instant::now() < deadline {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if contains_ci(&buf, b"</title") {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    if !buf.starts_with(b"HTTP/") || buf.len() < 12 {
        return Ok(None);
    }
    let status = std::str::from_utf8(&buf[9..12])
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let head_end = find(&buf, b"\r\n\r\n").map(|i| i + 4).unwrap_or(buf.len());
    let head = String::from_utf8_lossy(&buf[..head_end]);
    let location = head.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.eq_ignore_ascii_case("location")
            .then(|| v.trim().to_string())
    });
    Ok(Some(Resp {
        status,
        location,
        body: buf[head_end..].to_vec(),
    }))
}

fn same_port_path(location: &str, port: u16) -> Option<String> {
    if location.starts_with('/') {
        return Some(location.to_string());
    }
    let rest = location.strip_prefix("http://")?;
    let (host, path) = rest
        .split_once('/')
        .map(|(h, p)| (h, format!("/{p}")))
        .unwrap_or((rest, "/".into()));
    let local = [
        format!("localhost:{port}"),
        format!("127.0.0.1:{port}"),
        format!("[::1]:{port}"),
    ];
    local.contains(&host.to_string()).then_some(path)
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn contains_ci(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len())
        .any(|w| w.eq_ignore_ascii_case(needle))
}

fn extract_title(body: &[u8]) -> Option<String> {
    let lower: Vec<u8> = body.iter().map(|b| b.to_ascii_lowercase()).collect();
    let open = find(&lower, b"<title")?;
    let start = open + lower[open..].iter().position(|&b| b == b'>')? + 1;
    let end = start + find(&lower[start..], b"</title")?;
    let raw = String::from_utf8_lossy(&body[start..end]);
    let text = decode_entities(&raw)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if text.is_empty() {
        return None;
    }
    Some(text.chars().take(120).collect())
}

fn decode_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&nbsp;", " ")
        .replace("&middot;", "·")
        .replace("&mdash;", "—")
        .replace("&ndash;", "–")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{closed_port, html, http_server, TempDir};

    #[test]
    fn title_extraction() {
        let body = b"<html><head><TITLE data-x>\n  My &amp; App </title></head>";
        assert_eq!(extract_title(body).as_deref(), Some("My & App"));
        assert_eq!(extract_title(b"<html></html>"), None);
    }

    #[test]
    fn runtime_labels() {
        assert_eq!(
            runtime_label(
                "node /Users/x/app/node_modules/.bin/vite --port 5173",
                "node"
            ),
            "vite"
        );
        assert_eq!(
            runtime_label(
                "node /x/node_modules/next/dist/server/lib/start-server.js",
                "node"
            ),
            "next"
        );
        assert_eq!(
            runtime_label("/opt/homebrew/bin/python3.12 -m http.server 8000", "python"),
            "http.server"
        );
        assert_eq!(
            runtime_label("/usr/local/bin/postgres -D /data", "postgres"),
            "postgres"
        );
        assert_eq!(
            runtime_label("/opt/homebrew/bin/python3.12 app.py", "python"),
            "python"
        );
        assert_eq!(
            runtime_label("/Users/x/.bun/bin/bun.exe run dev", "bun"),
            "bun"
        );
        assert_eq!(runtime_label("/Applications/OrbStack.app/Contents/Frameworks/OrbStack Helper.app/Contents/MacOS/OrbStack Helper vmgr", "x"), "container");
    }

    #[test]
    fn redirects_stay_local() {
        assert_eq!(same_port_path("/login", 3000).as_deref(), Some("/login"));
        assert_eq!(
            same_port_path("http://localhost:3000/a", 3000).as_deref(),
            Some("/a")
        );
        assert_eq!(same_port_path("http://example.com/a", 3000), None);
    }

    #[test]
    fn system_processes() {
        assert!(is_system_process(
            "/System/Library/CoreServices/ControlCenter.app/Contents/MacOS/ControlCenter"
        ));
        assert!(is_system_process("/usr/libexec/rapportd"));
        assert!(!is_system_process("node /Users/x/app/server.js"));
        assert_eq!(
            exe_name("/Applications/Docker.app/Contents/MacOS/com.docker.backend --x"),
            "com.docker.backend"
        );
        assert!(is_desktop_app(
            "/Applications/Raycast.app/Contents/MacOS/Raycast"
        ));
        assert!(!is_desktop_app(
            "/Applications/Docker.app/Contents/MacOS/com.docker.backend"
        ));
        assert!(!is_desktop_app("node server.js"));
    }

    #[test]
    fn parses_lsof_listeners() {
        let out = "p123\ncnode\nf21\nn*:3000\nf22\nn[::1]:3000\n\
                   p456\ncpostgres\nf5\nn127.0.0.1:5432\nn[::1]:5432\nn127.0.0.1:5432\n\
                   p789\ncweird\nnnot-a-port\nn*:*\n\
                   p999\ncother\nn*:3000\n";
        let ls = parse_listeners(out);
        assert_eq!(ls.len(), 2);
        assert_eq!(
            (ls[0].port, ls[0].pid, ls[0].command.as_str()),
            (3000, 123, "node")
        );
        // A second process on the same port does not take it over, but its address merges.
        assert_eq!(ls[0].addrs, vec!["*", "[::1]"]);
        assert_eq!(
            (ls[1].port, ls[1].pid, ls[1].command.as_str()),
            (5432, 456, "postgres")
        );
        assert_eq!(ls[1].addrs, vec!["127.0.0.1", "[::1]"]);
        assert!(parse_listeners("").is_empty());
        // Unknown multi-byte tags are skipped rather than sliced mid-character.
        assert!(parse_listeners("é\nñ*:3000\n").is_empty());
    }

    #[test]
    fn parses_ps_args() {
        let m = parse_ps_args(
            "  123 node /x/server.js --port 3000\n4567 /usr/bin/python3 -m http.server  \ngarbage\nabc def\n",
        );
        assert_eq!(m.len(), 2);
        assert_eq!(m[&123], "node /x/server.js --port 3000");
        assert_eq!(m[&4567], "/usr/bin/python3 -m http.server");
    }

    #[test]
    fn parses_cwds() {
        let m = parse_cwds("p123\nfcwd\nn/Users/x/my app\np456\nfcwd\nn/\n");
        assert_eq!(m[&123], PathBuf::from("/Users/x/my app"));
        assert_eq!(m[&456], PathBuf::from("/"));
        assert_eq!(m.len(), 2);
    }

    #[test]
    fn empty_pid_lists_skip_the_subprocess() {
        assert!(process_args(&[]).is_empty());
        assert!(process_cwds(&[]).is_empty());
        assert_eq!(join_pids(&[1, 22, 333]), "1,22,333");
    }

    #[test]
    fn exe_paths_with_spaces_in_app_bundles() {
        let vscode = "/Applications/Visual Studio Code.app/Contents/Frameworks/Code Helper.app/Contents/MacOS/Code Helper --type=utility";
        assert_eq!(
            exe_path(vscode),
            "/Applications/Visual Studio Code.app/Contents/Frameworks/Code Helper.app/Contents/MacOS/Code Helper"
        );
        assert_eq!(exe_name(vscode), "Code Helper");
        assert!(is_desktop_app(vscode));
        assert_eq!(exe_path("node server.js --port 3000"), "node");
        assert_eq!(exe_path(""), "");
    }

    #[test]
    fn more_system_and_container_classification() {
        assert!(is_system_process("/usr/sbin/sshd -D"));
        assert!(is_system_process("/sbin/launchd"));
        assert!(!is_system_process("/usr/bin/python3 -m http.server"));
        assert!(!is_system_process(""));
        assert!(is_container_runtime(
            "docker-proxy -proto tcp -host-port 8080"
        ));
        assert!(is_container_runtime("/opt/homebrew/bin/limactl hostagent"));
        assert!(!is_container_runtime("node server.js"));
    }

    #[test]
    fn more_runtime_labels() {
        let cases = [
            ("python3 manage.py runserver 8000", "django"),
            ("/usr/bin/ruby bin/rails server", "rails"),
            ("puma 6.4.0 (tcp://localhost:3000) [app]", "puma"),
            (
                "/x/.venv/bin/python /x/.venv/bin/uvicorn main:app",
                "uvicorn",
            ),
            (
                "node /x/node_modules/webpack-dev-server/bin/webpack-dev-server.js",
                "webpack",
            ),
            ("node /x/node_modules/.bin/astro dev", "astro"),
            ("tsx watch src/index.ts", "tsx"),
            ("/usr/local/bin/redis-server *:6379", "redis-server"),
            ("/opt/homebrew/bin/node22 server.mjs", "node"),
            ("docker-proxy -proto tcp", "container"),
        ];
        for (args, want) in cases {
            assert_eq!(runtime_label(args, "fallback"), want, "{args}");
        }
        assert_eq!(runtime_label("", "node"), "node");
    }

    #[test]
    fn project_names_from_markers() {
        let tmp = TempDir::new();
        let home = tmp.path().join("home");
        let mk = |p: &str| std::fs::create_dir_all(home.join(p)).unwrap();
        let touch = |p: &str| std::fs::write(home.join(p), "").unwrap();
        mk("mono/.git");
        mk("mono/apps/web/src");
        touch("mono/apps/web/package.json");
        mk("solo/src/bin");
        touch("solo/Cargo.toml");
        mk("loose/dir");
        mk("repo/.git");

        let name = |p: &str| project_name(&home.join(p), &home);
        assert_eq!(name("mono/apps/web/src").as_deref(), Some("mono/web"));
        assert_eq!(name("mono/apps").as_deref(), Some("mono"));
        assert_eq!(name("solo/src/bin").as_deref(), Some("solo"));
        assert_eq!(name("repo").as_deref(), Some("repo"));
        assert_eq!(name("loose/dir"), None);
        assert_eq!(name(""), None, "home itself is never a project");
        assert_eq!(project_name(&home.join("solo"), Path::new("")), None);
        assert_eq!(project_name(Path::new("/opt/homebrew/var"), &home), None);
    }

    #[test]
    fn candidate_ips_map_wildcards_to_loopback() {
        let v4 = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let v6 = IpAddr::V6(Ipv6Addr::LOCALHOST);
        let ips = |a: &[&str]| candidate_ips(&a.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(ips(&["*", "0.0.0.0", "127.0.0.1"]), vec![v4]);
        assert_eq!(ips(&["[::]", "*"]), vec![v6, v4]);
        assert_eq!(
            ips(&["192.168.1.5", "[fe80::1]"]),
            vec![
                "192.168.1.5".parse::<IpAddr>().unwrap(),
                "fe80::1".parse::<IpAddr>().unwrap()
            ]
        );
        assert_eq!(ips(&["nonsense"]), vec![v4]);
        assert_eq!(ips(&[]), vec![v4]);
    }

    #[test]
    fn title_edge_cases() {
        assert_eq!(extract_title(b"<title></title>"), None);
        assert_eq!(extract_title(b"<title>   </title>"), None);
        assert_eq!(extract_title(b"<title>never closed"), None);
        assert_eq!(
            extract_title(
                b"<title>a &lt;b&gt; &quot;c&quot; &#39;d&#x27; e&nbsp;f &mdash; g</title>"
            )
            .as_deref(),
            Some("a <b> \"c\" 'd' e f \u{2014} g")
        );
        let long = format!("<title>{}</title>", "x".repeat(300));
        assert_eq!(extract_title(long.as_bytes()).unwrap().chars().count(), 120);
        assert!(contains_ci(b"..</TiTlE>", b"</title"));
        assert!(!contains_ci(b"short", b"much longer needle"));
        assert_eq!(find(b"abcabc", b"ca"), Some(2));
    }

    #[test]
    fn more_redirect_rules() {
        assert_eq!(
            same_port_path("http://127.0.0.1:3000", 3000).as_deref(),
            Some("/")
        );
        assert_eq!(
            same_port_path("http://[::1]:3000/x?y=1", 3000).as_deref(),
            Some("/x?y=1")
        );
        assert_eq!(same_port_path("http://localhost:4000/a", 3000), None);
        assert_eq!(same_port_path("https://localhost:3000/a", 3000), None);
        assert_eq!(same_port_path("login", 3000), None);
    }

    fn addrs(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn probe_reads_title_from_http_page() {
        let (port, _) = http_server(|_| html("200 OK", "Hello &amp; welcome"));
        let p = probe(port, &addrs(&["127.0.0.1"]));
        assert!(p.http);
        assert_eq!(p.status, 200);
        assert_eq!(p.title.as_deref(), Some("Hello & welcome"));
        assert_eq!(p.link_host, "localhost");
    }

    #[test]
    fn probe_follows_same_port_redirects() {
        let (port, hits) = http_server(|path| match path {
            "/" => b"HTTP/1.1 302 Found\r\nLocation: /login\r\nContent-Length: 0\r\n\r\n".to_vec(),
            _ => html("200 OK", "Sign in"),
        });
        let p = probe(port, &addrs(&["*"]));
        assert_eq!((p.status, p.title.as_deref()), (200, Some("Sign in")));
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[test]
    fn probe_does_not_follow_offsite_or_self_redirects() {
        let (port, _) = http_server(|_| {
            b"HTTP/1.1 301 Moved\r\nlocation: http://example.com/\r\n\r\n".to_vec()
        });
        let p = probe(port, &addrs(&["127.0.0.1"]));
        assert!(p.http);
        assert_eq!((p.status, p.title), (301, None));

        let (port, hits) = http_server(|_| b"HTTP/1.1 302 Found\r\nLocation: /\r\n\r\n".to_vec());
        assert_eq!(probe(port, &addrs(&["127.0.0.1"])).status, 302);
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn probe_gives_up_after_two_redirects_but_still_reports_http() {
        let (port, hits) = http_server(|path| {
            let next = path.trim_start_matches('/').parse::<u32>().unwrap_or(0) + 1;
            format!("HTTP/1.1 302 Found\r\nLocation: /{next}\r\n\r\n").into_bytes()
        });
        let p = probe(port, &addrs(&["127.0.0.1"]));
        assert!(p.http, "a redirect chain is still an HTTP service");
        assert_eq!(p.status, 302);
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[test]
    fn probe_reports_server_errors() {
        let (port, _) = http_server(|_| html("503 Service Unavailable", "Down"));
        let p = probe(port, &addrs(&["127.0.0.1"]));
        assert!(p.http);
        assert_eq!(p.status, 503);
    }

    #[test]
    fn probe_detects_non_http_listeners() {
        let (port, _) = http_server(|_| b"SSH-2.0-OpenSSH_9.6\r\n".to_vec());
        let p = probe(port, &addrs(&["127.0.0.1"]));
        assert!(!p.http);
        assert_eq!(p.link_host, "localhost");

        let (port, _) = http_server(|_| Vec::new());
        assert!(!probe(port, &addrs(&["127.0.0.1"])).http);
    }

    #[test]
    fn probe_falls_back_across_addresses_and_handles_closed_ports() {
        let (port, _) = http_server(|_| html("200 OK", "v4 only"));
        // Nothing on [::1]:port, so the probe moves on to the IPv4 address.
        let p = probe(port, &addrs(&["[::1]", "127.0.0.1"]));
        assert_eq!(p.title.as_deref(), Some("v4 only"));

        let p = probe(closed_port(), &addrs(&["127.0.0.1"]));
        assert!(!p.http);
        assert_eq!(p.link_host, "localhost");
    }
}
