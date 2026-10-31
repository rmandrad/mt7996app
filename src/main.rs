mod collector;
mod debugfs;
mod iwparse;
mod model;
mod server;
mod sources;

use collector::Collector;
use model::{Source, State};
use sources::{DemoSource, RealSource};
use std::time::Duration;

const HELP: &str = "\
mt7996app - MLO-aware live dashboard for mt7996 (Wi-Fi 7) access points

USAGE: mt7996app [OPTIONS]

  --listen ADDR:PORT  where the web server listens, e.g. 0.0.0.0:8080,
                    192.168.1.1:8996, [::1]:8996 or just :8080 (keeps the bind address)
  --bind ADDR       listen address only (default 127.0.0.1; the UI has no auth)
  --port N          listen port only (default 8996)
  --interval SECS   poll interval (default 2)
  --demo            synthetic data, no hardware needed
  --any-driver      include non-mt7996 wifi phys
  --iw PATH         path to the iw binary (default: iw)
  --debugfs PATH    debugfs ieee80211 root (default: /sys/kernel/debug/ieee80211)
  --once            sample twice, print a text summary and exit
  --json            with --once: print JSON instead
  -h, --help / -V, --version

ENVIRONMENT (overridden by command-line options):
  MT7996APP_LISTEN=ADDR:PORT   same as --listen
  MT7996APP_BIND=ADDR          same as --bind
  MT7996APP_PORT=N             same as --port
";

struct Args {
    bind: String,
    port: u16,
    interval: f64,
    demo: bool,
    any_driver: bool,
    iw: String,
    debugfs: String,
    once: bool,
    json: bool,
}

/// Parse `ADDR:PORT`, `[V6]:PORT`, `:PORT` or a bare `PORT` into (bind, port).
/// An empty address keeps `default_bind`.
fn parse_listen(s: &str, default_bind: &str) -> Result<(String, u16), String> {
    let bad = || format!("bad listen address '{s}' (expected ADDR:PORT, [IPV6]:PORT or :PORT)");
    if let Ok(sa) = s.parse::<std::net::SocketAddr>() {
        return Ok((sa.ip().to_string(), sa.port()));
    }
    let (host, port) = match s.rsplit_once(':') {
        Some((h, p)) => (h.trim_matches(|c| c == '[' || c == ']'), p),
        None => ("", s),
    };
    let port: u16 = port.parse().map_err(|_| bad())?;
    let host = if host.is_empty() { default_bind } else { host };
    Ok((host.to_string(), port))
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args {
        bind: "127.0.0.1".into(),
        port: 8996,
        interval: 2.0,
        demo: false,
        any_driver: false,
        iw: "iw".into(),
        debugfs: debugfs::DEFAULT_ROOT.into(),
        once: false,
        json: false,
    };
    // Environment first, so command-line options override it.
    if let Ok(v) = std::env::var("MT7996APP_LISTEN") {
        (a.bind, a.port) = parse_listen(&v, &a.bind)?;
    }
    if let Ok(v) = std::env::var("MT7996APP_BIND") {
        a.bind = v;
    }
    if let Ok(v) = std::env::var("MT7996APP_PORT") {
        a.port = v.parse().map_err(|_| "bad MT7996APP_PORT")?;
    }
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut val = |name: &str| it.next().ok_or_else(|| format!("{name} needs a value"));
        match arg.as_str() {
            "--listen" => (a.bind, a.port) = parse_listen(&val("--listen")?, &a.bind)?,
            "--bind" => a.bind = val("--bind")?,
            "--port" => a.port = val("--port")?.parse().map_err(|_| "bad --port")?,
            "--interval" => {
                a.interval = val("--interval")?.parse().map_err(|_| "bad --interval")?;
                if !(a.interval >= 0.2) {
                    return Err("--interval must be >= 0.2".into());
                }
            }
            "--iw" => a.iw = val("--iw")?,
            "--debugfs" => a.debugfs = val("--debugfs")?,
            "--demo" => a.demo = true,
            "--any-driver" => a.any_driver = true,
            "--once" => a.once = true,
            "--json" => a.json = true,
            "-h" | "--help" => {
                print!("{HELP}");
                std::process::exit(0);
            }
            "-V" | "--version" => {
                println!("mt7996app {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            other => return Err(format!("unknown option {other} (see --help)")),
        }
    }
    Ok(a)
}

fn opt<T: std::fmt::Display>(v: Option<T>) -> String {
    v.map_or("-".into(), |x| x.to_string())
}

fn text_summary(st: &State) -> String {
    let mut out = vec![];
    for r in &st.radios {
        out.push(format!(
            "{:<8} link {:<4} {:<4} ch{:<4} {:>4} MHz  tx {} dBm  noise {}  busy {}%  sta {}",
            r.iface,
            opt(r.link),
            opt(r.band),
            opt(r.channel),
            opt(r.width),
            opt(r.txpower),
            opt(r.noise),
            opt(r.busy_pct),
            r.stations
        ));
    }
    out.push(String::new());
    for s in &st.stations {
        out.push(format!(
            "{} {}{} sig {} dBm  tx {} Mb/s  rx {} Mb/s  retry {}%",
            s.mac,
            s.iface,
            if s.mlo { " [MLO]" } else { "" },
            opt(s.stats.signal),
            opt(s.stats.mbps_tx),
            opt(s.stats.mbps_rx),
            opt(s.stats.retry_pct)
        ));
        for l in &s.links {
            out.push(format!(
                "    link {} {:<4} sig {}  tx {} Mb/s  retry {}%",
                l.id,
                opt(l.band),
                opt(l.stats.signal),
                opt(l.stats.mbps_tx),
                opt(l.stats.retry_pct)
            ));
        }
    }
    out.extend(st.errors.iter().map(|e| format!("! {e}")));
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::parse_listen;

    #[test]
    fn listen_forms() {
        let p = |s| parse_listen(s, "127.0.0.1");
        assert_eq!(p("0.0.0.0:8080"), Ok(("0.0.0.0".into(), 8080)));
        assert_eq!(p("192.168.1.1:8996"), Ok(("192.168.1.1".into(), 8996)));
        assert_eq!(p("[::1]:9000"), Ok(("::1".into(), 9000)));
        assert_eq!(p(":8080"), Ok(("127.0.0.1".into(), 8080)));
        assert_eq!(p("8080"), Ok(("127.0.0.1".into(), 8080)));
        assert_eq!(p("router.lan:80"), Ok(("router.lan".into(), 80)));
        assert!(p("1.2.3.4:").is_err());
        assert!(p("1.2.3.4:99999").is_err());
        assert!(p("nonsense").is_err());
    }
}

fn main() {
    let a = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("mt7996app: {e}");
            std::process::exit(2);
        }
    };
    let src: Box<dyn Source> = if a.demo {
        Box::new(DemoSource::new())
    } else {
        Box::new(RealSource::new(a.iw.clone(), a.debugfs.clone(), a.any_driver))
    };
    let col = Collector::new(src, a.interval);

    if a.once {
        col.poll_once();
        std::thread::sleep(Duration::from_secs_f64(a.interval.min(2.0)));
        let st = col.poll_once();
        if a.json {
            println!("{}", serde_json::to_string_pretty(&st).unwrap());
        } else {
            println!("{}", text_summary(&st));
        }
        return;
    }

    col.start();
    if let Err(e) = server::serve(col, &a.bind, a.port, a.demo) {
        eprintln!("mt7996app: {e}");
        std::process::exit(1);
    }
}
