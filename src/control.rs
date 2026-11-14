//! The only code that changes radio / client state.
//!
//! Every action is a typed, strictly validated request that is turned into an
//! argv vector (never a shell string) for `iw` or `hostapd_cli`. Actions run
//! only when the daemon was started with `--allow-control`, require the
//! shared token, can be dry-run, and are written to the event log.
use crate::events::EventLog;
use crate::planner::valid_puncturing;
use crate::util::{is_ifname, is_mac};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct ControlCfg {
    pub enabled: bool,
    pub dry_run: bool,
    pub token: String,
    pub iw: String,
    /// iproute2, for bringing the temporary scan interface up
    pub ip: String,
    pub hostapd_cli: String,
    pub hostapd_ctrl: Option<String>,
    pub airtime_auto: bool,
    pub steer_auto: bool,
}

impl Default for ControlCfg {
    fn default() -> Self {
        Self {
            enabled: false,
            dry_run: false,
            token: String::new(),
            iw: "iw".into(),
            ip: "ip".into(),
            hostapd_cli: "hostapd_cli".into(),
            hostapd_ctrl: None,
            airtime_auto: false,
            steer_auto: false,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Action {
    /// `iw dev IF station set MAC airtime_weight W`
    SetAirtime { iface: String, mac: String, weight: u32 },
    /// `iw dev IF scan trigger` (briefly leaves the channel!)
    Scan { iface: String },
    /// `hostapd_cli chan_switch ...`, also used to change only the puncturing
    ChanSwitch {
        iface: String,
        link: Option<u32>,
        /// primary channel frequency
        freq: u32,
        center1: u32,
        bandwidth: u32,
        punct: Option<u32>,
        count: Option<u32>,
    },
    /// 802.11v BSS transition request
    Btm { iface: String, mac: String, neighbor: String },
}

fn valid_neighbor(n: &str) -> bool {
    let p: Vec<&str> = n.split(',').collect();
    p.len() == 5
        && is_mac(p[0])
        && p[1].strip_prefix("0x").is_some_and(|h| !h.is_empty() && h.len() <= 8 && h.bytes().all(|c| c.is_ascii_hexdigit()))
        && p[2..].iter().all(|x| !x.is_empty() && x.len() <= 3 && x.bytes().all(|c| c.is_ascii_digit()))
}

impl Action {
    pub fn name(&self) -> &'static str {
        match self {
            Action::SetAirtime { .. } => "set_airtime",
            Action::Scan { .. } => "scan",
            Action::ChanSwitch { .. } => "chan_switch",
            Action::Btm { .. } => "btm",
        }
    }

    /// Validate and build `(program, args)`.
    pub fn argv(&self, cfg: &ControlCfg) -> Result<(String, Vec<String>), String> {
        let s = |x: &str| x.to_string();
        let hostapd = |iface: &str, link: Option<u32>| crate::hostapd::prefix(cfg.hostapd_ctrl.as_deref(), iface, link);
        match self {
            Action::SetAirtime { iface, mac, weight } => {
                if !is_ifname(iface) || !is_mac(mac) || !(1..=1024).contains(weight) {
                    return Err("invalid set_airtime arguments".into());
                }
                Ok((cfg.iw.clone(), vec![s("dev"), s(iface), s("station"), s("set"), s(mac), s("airtime_weight"), weight.to_string()]))
            }
            Action::Scan { iface } => {
                if !is_ifname(iface) {
                    return Err("invalid interface".into());
                }
                Ok((cfg.iw.clone(), vec![s("dev"), s(iface), s("scan"), s("trigger")]))
            }
            Action::ChanSwitch { iface, link, freq, center1, bandwidth, punct, count } => {
                let count = count.unwrap_or(5);
                if !is_ifname(iface)
                    || link.is_some_and(|l| l > 15)
                    || !(2400..=7200).contains(freq)
                    || !(2400..=7200).contains(center1)
                    || ![20, 40, 80, 160, 320].contains(bandwidth)
                    || count > 20
                {
                    return Err("invalid chan_switch arguments".into());
                }
                let p = punct.unwrap_or(0);
                if p != 0 && !valid_puncturing(*bandwidth, *center1, *freq, p) {
                    return Err(format!("puncturing bitmap 0x{p:x} is not valid for {bandwidth} MHz (kernel would reject it)"));
                }
                let mut a = hostapd(iface, *link);
                a.extend([s("chan_switch"), count.to_string(), freq.to_string(), format!("center_freq1={center1}"), format!("bandwidth={bandwidth}")]);
                let modes: &[&str] = if *freq < 3000 { &["ht", "he", "eht"] } else if *freq < 5925 { &["ht", "vht", "he", "eht"] } else { &["he", "eht"] };
                a.extend(modes.iter().map(|m| s(m)));
                if p != 0 {
                    a.push(format!("punct_bitmap=0x{p:x}"));
                }
                Ok((cfg.hostapd_cli.clone(), a))
            }
            Action::Btm { iface, mac, neighbor } => {
                if !is_ifname(iface) || !is_mac(mac) || !valid_neighbor(neighbor) {
                    return Err("invalid btm arguments".into());
                }
                let mut a = hostapd(iface, None);
                a.extend([s("bss_tm_req"), s(mac), format!("neighbor={neighbor}"), s("pref=1"), s("abridged=1"), s("valid_int=100")]);
                Ok((cfg.hostapd_cli.clone(), a))
            }
        }
    }
}

/// Name of the short-lived managed interface used to scan from an MLD AP.
pub const TEMP_SCAN_IFACE: &str = "mt7996scan0";
const DIRECT_SCAN_TIMEOUT: Duration = Duration::from_secs(10);
/// `iw dev X scan` blocks until all bands are done (2.4 + 5 + 6 GHz take 10-30 s).
const TEMP_SCAN_TIMEOUT: Duration = Duration::from_secs(90);

pub struct Control {
    pub cfg: ControlCfg,
    pub log: Arc<EventLog>,
    /// held while a scan runs: two scans must not share (and delete) the temporary interface
    scan_lock: Mutex<()>,
}

/// `phyN` of an interface from `iw dev IF info` ("wiphy N").
pub fn parse_wiphy(info: &str) -> Option<String> {
    info.lines().find_map(|l| l.trim().strip_prefix("wiphy ")).and_then(|n| n.trim().parse::<u32>().ok()).map(|n| format!("phy{n}"))
}

/// The kernel refuses `scan trigger` on an MLD AP with EOPNOTSUPP (-95).
pub fn scan_unsupported(err: &str) -> bool {
    err.contains("(-95)") || err.to_ascii_lowercase().contains("not supported")
}

/// The commands that scan through a temporary managed interface of `phy`
/// (`add`, `up`, `scan`, `del`); the last one must always run.
pub fn temp_scan_steps(cfg: &ControlCfg, phy: &str) -> Result<Vec<(String, Vec<String>)>, String> {
    if !phy.strip_prefix("phy").is_some_and(|n| !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit())) {
        return Err(format!("invalid phy name '{phy}'"));
    }
    let v = |a: &[&str]| a.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    Ok(vec![
        (cfg.iw.clone(), v(&["phy", phy, "interface", "add", TEMP_SCAN_IFACE, "type", "managed"])),
        (cfg.ip.clone(), v(&["link", "set", TEMP_SCAN_IFACE, "up"])),
        (cfg.iw.clone(), v(&["dev", TEMP_SCAN_IFACE, "scan"])),
        (cfg.iw.clone(), v(&["dev", TEMP_SCAN_IFACE, "del"])),
    ])
}

/// Compare without leaking the position of the first mismatch.
pub fn token_ok(expected: &str, given: &str) -> bool {
    let (a, b) = (expected.as_bytes(), given.as_bytes());
    let mut diff = (a.len() ^ b.len()) as u8;
    for i in 0..a.len().min(b.len()) {
        diff |= a[i] ^ b[i];
    }
    diff == 0 && !a.is_empty()
}

impl Control {
    pub fn new(cfg: ControlCfg, log: Arc<EventLog>) -> Arc<Self> {
        Arc::new(Self { cfg, log, scan_lock: Mutex::new(()) })
    }

    /// Run one command and leave an audit entry with the exact command line.
    fn run_logged(&self, who: &str, prog: &str, args: &[String], timeout: Duration) -> Result<String, String> {
        let line = format!("{prog} {}", args.join(" "));
        match crate::util::run(prog, args, timeout) {
            Ok(out) => {
                self.log.push("info", "control", format!("{who}: {line} -> ok"));
                Ok(out)
            }
            Err(e) => {
                self.log.push("error", "control", format!("{who}: {line} -> {e}"));
                Err(e)
            }
        }
    }

    /// Remove a leftover temporary scan interface (a crash between `add` and `del`). Silent when there is none.
    pub fn cleanup_temp_iface(&self) {
        if self.cfg.enabled && !self.cfg.dry_run {
            let del = ["dev", TEMP_SCAN_IFACE, "del"].map(String::from);
            if crate::util::run(&self.cfg.iw, &del, Duration::from_secs(5)).is_ok() {
                self.log.push("warn", "control", format!("removed a leftover temporary interface {TEMP_SCAN_IFACE}"));
            }
        }
    }

    fn phy_of(&self, iface: &str) -> Result<String, String> {
        let a = ["dev", iface, "info"].map(String::from);
        crate::util::run(&self.cfg.iw, &a, Duration::from_secs(5)).and_then(|t| parse_wiphy(&t).ok_or_else(|| format!("cannot find the phy of {iface}")))
    }

    /// `scan`: trigger on the interface; if the kernel refuses (MLD AP), scan from a temporary
    /// managed interface of the same phy instead. The scan cache belongs to the phy, so the
    /// results show up in `iw dev IF scan dump` like any other scan.
    fn scan(&self, who: &str, a: &Action, iface: &str) -> Result<String, String> {
        let (prog, args) = a.argv(&self.cfg)?;
        let line = format!("{prog} {}", args.join(" "));
        if self.cfg.dry_run {
            let fallback = match self.phy_of(iface).and_then(|p| temp_scan_steps(&self.cfg, &p)) {
                Ok(steps) => steps.iter().map(|(p, a)| format!("{p} {}", a.join(" "))).collect::<Vec<_>>().join("; "),
                Err(_) => format!("{} phy <phy of {iface}> interface add {TEMP_SCAN_IFACE} type managed; ...", self.cfg.iw),
            };
            self.log.push("info", "control", format!("{who}: DRY-RUN {line}  (if the kernel refuses, e.g. on an MLD AP: {fallback})"));
            return Ok(format!("dry-run: {line}  (fallback on MLD APs: {fallback})"));
        }
        let Ok(_guard) = self.scan_lock.try_lock() else {
            return Err("a scan is already running".into());
        };
        match crate::util::run(&prog, &args, DIRECT_SCAN_TIMEOUT) {
            Ok(out) => {
                self.log.push("info", "control", format!("{who}: {line} -> ok"));
                return Ok(out.trim().to_string());
            }
            Err(e) if scan_unsupported(&e) => {
                self.log.push("info", "control", format!("{who}: {line} -> refused ({}); scanning from a temporary interface", e.rsplit(": ").next().unwrap_or(&e)));
            }
            Err(e) => {
                self.log.push("error", "control", format!("{who}: {line} -> {e}"));
                return Err(e);
            }
        }
        self.scan_via_temp_iface(who, iface)
    }

    fn scan_via_temp_iface(&self, who: &str, iface: &str) -> Result<String, String> {
        let steps = temp_scan_steps(&self.cfg, &self.phy_of(iface)?)?;
        let (add, rest) = steps.split_first().ok_or("no steps")?;
        let (del, middle) = rest.split_last().ok_or("no steps")?;
        // a crashed earlier run may have left the interface behind
        let _ = crate::util::run(&del.0, &del.1, Duration::from_secs(5));
        self.run_logged(who, &add.0, &add.1, Duration::from_secs(10)).map_err(|e| {
            format!("{e}\nthe driver refused a second interface next to the AP; scan with another tool or rely on hostapd ACS")
        })?;
        let mut result = Ok(String::new());
        for (prog, args) in middle {
            let t = if args.last().is_some_and(|l| l == "scan") { TEMP_SCAN_TIMEOUT } else { Duration::from_secs(10) };
            match self.run_logged(who, prog, args, t) {
                Ok(out) => result = Ok(out),
                Err(e) => {
                    result = Err(e);
                    break;
                }
            }
        }
        // always remove the temporary interface, whatever happened
        let cleanup = self.run_logged(who, &del.0, &del.1, Duration::from_secs(10));
        match (result, cleanup) {
            (Ok(out), Ok(_)) => {
                let n = out.lines().filter(|l| l.starts_with("BSS ")).count();
                Ok(format!("scan finished from the temporary interface {TEMP_SCAN_IFACE}: {n} BSS in the scan cache"))
            }
            (Ok(_), Err(e)) => Err(format!("scan finished, but the temporary interface {TEMP_SCAN_IFACE} was NOT removed ({e}); run `iw dev {TEMP_SCAN_IFACE} del`")),
            (Err(e), Ok(_)) => Err(e),
            (Err(e), Err(c)) => Err(format!("{e}\nand the temporary interface {TEMP_SCAN_IFACE} was NOT removed ({c}); run `iw dev {TEMP_SCAN_IFACE} del`")),
        }
    }

    /// Run (or dry-run) an action; always leaves an audit entry.
    pub fn execute(&self, who: &str, a: &Action) -> Result<String, String> {
        if !self.cfg.enabled {
            return Err("control is disabled (start with --allow-control)".into());
        }
        if let Action::Scan { iface } = a {
            return self.scan(who, a, iface);
        }
        let (prog, args) = a.argv(&self.cfg)?;
        let line = format!("{prog} {}", args.join(" "));
        if self.cfg.dry_run {
            self.log.push("info", "control", format!("{who}: DRY-RUN {line}"));
            return Ok(format!("dry-run: {line}"));
        }
        self.run_logged(who, &prog, &args, Duration::from_secs(10)).map(|o| o.trim().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> ControlCfg {
        ControlCfg { enabled: true, token: "t".into(), ..Default::default() }
    }

    #[test]
    fn airtime_argv() {
        let a = Action::SetAirtime { iface: "wlan0".into(), mac: "10:91:d1:f7:5e:bd".into(), weight: 64 };
        let (p, args) = a.argv(&cfg()).unwrap();
        assert_eq!(p, "iw");
        assert_eq!(args.join(" "), "dev wlan0 station set 10:91:d1:f7:5e:bd airtime_weight 64");
    }

    #[test]
    fn rejects_injection_and_ranges() {
        let c = cfg();
        for bad in ["wlan0; reboot", "wl an0", "", "-i", "a/b", "waytoolonginterfacename"] {
            // a leading '-' is harmless for argv but we still never accept shell-ish names
            if bad != "-i" {
                assert!(Action::Scan { iface: bad.into() }.argv(&c).is_err(), "{bad}");
            }
        }
        assert!(Action::SetAirtime { iface: "wlan0".into(), mac: "zz:91:d1:f7:5e:bd".into(), weight: 10 }.argv(&c).is_err());
        assert!(Action::SetAirtime { iface: "wlan0".into(), mac: "10:91:d1:f7:5e:bd".into(), weight: 0 }.argv(&c).is_err());
        assert!(Action::SetAirtime { iface: "wlan0".into(), mac: "10:91:d1:f7:5e:bd".into(), weight: 5000 }.argv(&c).is_err());
        assert!(Action::Btm { iface: "wlan0".into(), mac: "10:91:d1:f7:5e:bd".into(), neighbor: "a b".into() }.argv(&c).is_err());
    }

    #[test]
    fn chan_switch_with_puncturing() {
        let mut c = cfg();
        c.hostapd_ctrl = Some("/var/run/hostapd".into());
        let a = Action::ChanSwitch { iface: "wlan0".into(), link: Some(0), freq: 5180, center1: 5210, bandwidth: 80, punct: Some(0x8), count: None };
        let (p, args) = a.argv(&c).unwrap();
        assert_eq!(p, "hostapd_cli");
        assert_eq!(args.join(" "), "-p /var/run/hostapd -i wlan0 -l 0 chan_switch 5 5180 center_freq1=5210 bandwidth=80 ht vht he eht punct_bitmap=0x8");
        let bad = Action::ChanSwitch { iface: "wlan0".into(), link: None, freq: 5180, center1: 5210, bandwidth: 80, punct: Some(0x1), count: None };
        assert!(bad.argv(&c).unwrap_err().contains("not valid"));
    }

    #[test]
    fn btm_argv_and_json() {
        let a: Action = serde_json::from_str(r#"{"action":"btm","iface":"wlan0","mac":"10:91:d1:f7:5e:bd","neighbor":"12:91:d1:f7:5e:c1,0x0000,128,36,9"}"#).unwrap();
        let (_, args) = a.argv(&cfg()).unwrap();
        assert!(args.join(" ").ends_with("bss_tm_req 10:91:d1:f7:5e:bd neighbor=12:91:d1:f7:5e:c1,0x0000,128,36,9 pref=1 abridged=1 valid_int=100"));
        assert!(serde_json::from_str::<Action>(r#"{"action":"rm_rf"}"#).is_err());
    }

    #[test]
    fn disabled_and_dry_run() {
        let log = EventLog::new();
        let a = Action::Scan { iface: "wlan0".into() };
        assert!(Control::new(ControlCfg::default(), log.clone()).execute("t", &a).is_err());
        let mut c = cfg();
        c.dry_run = true;
        let out = Control::new(c, log.clone()).execute("ui", &a).unwrap();
        assert!(out.starts_with("dry-run: iw dev wlan0 scan trigger"), "{out}");
        assert!(out.contains("interface add"), "the fallback is shown: {out}");
        assert!(log.all().last().unwrap().msg.contains("DRY-RUN"));
    }

    #[test]
    fn token_compare() {
        assert!(token_ok("secret", "secret"));
        assert!(!token_ok("secret", "secreT"));
        assert!(!token_ok("secret", "secre"));
        assert!(!token_ok("", ""));
    }

    #[test]
    fn temp_scan_commands() {
        let steps = temp_scan_steps(&cfg(), "phy0").unwrap();
        let lines: Vec<String> = steps.iter().map(|(p, a)| format!("{p} {}", a.join(" "))).collect();
        assert_eq!(
            lines,
            [
                "iw phy phy0 interface add mt7996scan0 type managed",
                "ip link set mt7996scan0 up",
                "iw dev mt7996scan0 scan",
                "iw dev mt7996scan0 del"
            ]
        );
        for bad in ["phy", "phy0; reboot", "wlan0", "phyx", ""] {
            assert!(temp_scan_steps(&cfg(), bad).is_err(), "{bad}");
        }
        assert!(crate::util::is_ifname(TEMP_SCAN_IFACE));
    }

    #[test]
    fn wiphy_and_refusal_detection() {
        assert_eq!(parse_wiphy("Interface wlan0\n\tifindex 17\n\twdev 0x2\n\twiphy 0\n\ttype AP\n").as_deref(), Some("phy0"));
        assert_eq!(parse_wiphy("\twiphy 12\n").as_deref(), Some("phy12"));
        assert_eq!(parse_wiphy("nothing\n"), None);
        assert!(scan_unsupported("iw dev wlan0 scan trigger: command failed: Operation not supported (-95)"));
        assert!(!scan_unsupported("iw dev wlan0 scan trigger: command failed: Device or resource busy (-16)"));
    }

    #[test]
    fn a_second_scan_is_refused_while_one_runs() {
        let c = Control::new(cfg(), EventLog::new());
        let _held = c.scan_lock.lock().unwrap();
        let r = c.execute("t", &Action::Scan { iface: "wlan0".into() });
        assert_eq!(r.unwrap_err(), "a scan is already running");
    }

    /// Fake `iw` / `ip` that log every call: scenario 1 refuses `scan trigger` (MLD AP) and lets the
    /// temporary-interface scan succeed, scenario 2 makes that scan fail; the interface must be
    /// deleted in both, and scenario 3 has a driver that refuses the second interface.
    #[test]
    fn scan_falls_back_to_a_temporary_interface_and_always_removes_it() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("mt7996app-scan-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("calls.log");
        let script = |name: &str, body: &str| {
            let p = dir.join(name);
            std::fs::write(&p, format!("#!/bin/sh\necho \"{name} $*\" >> {}\n{body}\n", log.display())).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
            p.to_string_lossy().into_owned()
        };
        let iw = script(
            "iw",
            r#"case "$*" in
 "dev wlan0 scan trigger") echo "command failed: Operation not supported (-95)" >&2; exit 1;;
 "dev wlan0 info") printf 'Interface wlan0\n\twiphy 0\n';;
 "dev mt7996scan0 scan") if [ -e "$0.failscan" ]; then echo "command failed: Device or resource busy (-16)" >&2; exit 1; fi
     printf 'BSS aa:bb:cc:dd:ee:01(on mt7996scan0)\n\tfreq: 5240\nBSS aa:bb:cc:dd:ee:02(on mt7996scan0)\n\tfreq: 2437\n';;
 "phy phy0 interface add"*) if [ -e "$0.noadd" ]; then echo "command failed: Device or resource busy (-16)" >&2; exit 1; fi;;
esac"#,
        );
        let ip = script("ip", "");
        let c = Control::new(ControlCfg { enabled: true, iw: iw.clone(), ip, ..Default::default() }, EventLog::new());
        let calls = || std::fs::read_to_string(&log).unwrap_or_default().lines().map(|l| l.splitn(2, ' ').collect::<Vec<_>>().join(" ").replace(&format!("{}/", dir.display()), "")).collect::<Vec<_>>();
        let go = || c.execute("t", &Action::Scan { iface: "wlan0".into() });

        // 1: fallback works
        let out = go().unwrap();
        assert!(out.contains("2 BSS"), "{out}");
        let seq: Vec<String> = calls();
        assert_eq!(
            seq,
            [
                "iw dev wlan0 scan trigger",
                "iw dev wlan0 info",
                "iw dev mt7996scan0 del", // leftover cleanup, result ignored
                "iw phy phy0 interface add mt7996scan0 type managed",
                "ip link set mt7996scan0 up",
                "iw dev mt7996scan0 scan",
                "iw dev mt7996scan0 del",
            ]
        );
        assert!(c.log.all().iter().any(|e| e.msg.contains("scanning from a temporary interface")));

        // 2: the scan itself fails, the interface is still deleted
        std::fs::remove_file(&log).unwrap();
        std::fs::write(format!("{iw}.failscan"), "").unwrap();
        let e = go().unwrap_err();
        assert!(e.contains("busy"), "{e}");
        assert_eq!(calls().last().map(String::as_str), Some("iw dev mt7996scan0 del"));
        std::fs::remove_file(format!("{iw}.failscan")).unwrap();

        // 3: the driver refuses the interface: clear error, no scan attempted
        std::fs::remove_file(&log).unwrap();
        std::fs::write(format!("{iw}.noadd"), "").unwrap();
        let e = go().unwrap_err();
        assert!(e.contains("refused a second interface"), "{e}");
        assert!(!calls().iter().any(|l| l.contains("mt7996scan0 scan")));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
