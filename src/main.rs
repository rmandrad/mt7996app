mod airtime;
mod collector;
mod control;
mod debugfs;
mod events;
mod hostapd;
mod iwparse;
mod model;
mod nl80211;
mod planner;
mod server;
mod sources;
mod steering;
mod util;

use collector::Collector;
use control::{Control, ControlCfg};
use events::EventLog;
use hostapd::HostapdCfg;
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
  --no-netlink      do not read per-link statistics from nl80211 (iw only)
  --no-kmsg         do not follow /dev/kmsg for DFS/firmware events
  --hostapd-cli P   hostapd_cli binary (default: hostapd_cli); used for DFS state
  --hostapd-ctrl D  hostapd control socket directory (hostapd_cli -p D)

CONTROL (off by default; changes radio / client state - see README):
  --allow-control          enable POST /api/control and the auto modes
  --control-token-file F   file holding the shared token (>= 8 chars);
                           or set MT7996APP_TOKEN. Required with --allow-control
  --control-dry-run        log the commands instead of running them
  --airtime-auto           lower the airtime weight of clients hogging airtime
  --steer-auto             send BSS transition requests to single-link clients
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
    netlink: bool,
    kmsg: bool,
    hostapd_cli: String,
    hostapd_ctrl: Option<String>,
    allow_control: bool,
    token_file: Option<String>,
    dry_run: bool,
    airtime_auto: bool,
    steer_auto: bool,
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
        netlink: true,
        kmsg: true,
        hostapd_cli: "hostapd_cli".into(),
        hostapd_ctrl: None,
        allow_control: false,
        token_file: None,
        dry_run: false,
        airtime_auto: false,
        steer_auto: false,
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
            "--no-netlink" => a.netlink = false,
            "--no-kmsg" => a.kmsg = false,
            "--hostapd-cli" => a.hostapd_cli = val("--hostapd-cli")?,
            "--hostapd-ctrl" => a.hostapd_ctrl = Some(val("--hostapd-ctrl")?),
            "--allow-control" => a.allow_control = true,
            "--control-token-file" => a.token_file = Some(val("--control-token-file")?),
            "--control-dry-run" => a.dry_run = true,
            "--airtime-auto" => a.airtime_auto = true,
            "--steer-auto" => a.steer_auto = true,
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
    if (a.airtime_auto || a.steer_auto || a.dry_run) && !a.allow_control {
        return Err("--airtime-auto, --steer-auto and --control-dry-run need --allow-control".into());
    }
    Ok(a)
}

/// Token from `--control-token-file` or `MT7996APP_TOKEN`.
fn load_token(a: &Args) -> Result<String, String> {
    let t = match &a.token_file {
        Some(f) => std::fs::read_to_string(f).map_err(|e| format!("cannot read token file {f}: {e}"))?,
        None => std::env::var("MT7996APP_TOKEN").map_err(|_| "--allow-control needs --control-token-file FILE or MT7996APP_TOKEN".to_string())?,
    };
    let t = t.trim().to_string();
    if t.len() < 8 {
        return Err("the control token must be at least 8 characters".into());
    }
    Ok(t)
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
    let hostapd = Some(HostapdCfg { cli: a.hostapd_cli.clone(), ctrl: a.hostapd_ctrl.clone() });
    let src: Box<dyn Source> = if a.demo {
        Box::new(DemoSource::new())
    } else {
        Box::new(RealSource::new(a.iw.clone(), a.debugfs.clone(), a.any_driver, a.netlink, hostapd))
    };
    let log = EventLog::new();
    let token = if a.allow_control {
        match load_token(&a) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("mt7996app: {e}");
                std::process::exit(2);
            }
        }
    } else {
        String::new()
    };
    let ctl = Control::new(
        ControlCfg {
            enabled: a.allow_control,
            dry_run: a.dry_run,
            token,
            iw: a.iw.clone(),
            hostapd_cli: a.hostapd_cli.clone(),
            hostapd_ctrl: a.hostapd_ctrl.clone(),
            airtime_auto: a.airtime_auto,
            steer_auto: a.steer_auto,
        },
        log.clone(),
    );
    if a.kmsg && !a.demo && !a.once {
        events::spawn_kmsg(log.clone());
    }
    let col = Collector::new(src, a.interval, ctl, log.clone());

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
    if let Err(e) = server::serve(col, log, &a.bind, a.port, a.demo) {
        eprintln!("mt7996app: {e}");
        std::process::exit(1);
    }
}
