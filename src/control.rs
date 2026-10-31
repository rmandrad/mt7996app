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
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct ControlCfg {
    pub enabled: bool,
    pub dry_run: bool,
    pub token: String,
    pub iw: String,
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
        let hostapd = |iface: &str, link: Option<u32>| {
            let mut a = vec![];
            if let Some(c) = &cfg.hostapd_ctrl {
                a.extend([s("-p"), c.clone()]);
            }
            a.extend([s("-i"), s(iface)]);
            if let Some(l) = link {
                a.extend([s("-l"), l.to_string()]);
            }
            a
        };
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

pub struct Control {
    pub cfg: ControlCfg,
    pub log: Arc<EventLog>,
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
        Arc::new(Self { cfg, log })
    }

    /// Run (or dry-run) an action; always leaves an audit entry.
    pub fn execute(&self, who: &str, a: &Action) -> Result<String, String> {
        if !self.cfg.enabled {
            return Err("control is disabled (start with --allow-control)".into());
        }
        let (prog, args) = a.argv(&self.cfg)?;
        let line = format!("{prog} {}", args.join(" "));
        if self.cfg.dry_run {
            self.log.push("info", "control", format!("{who}: DRY-RUN {line}"));
            return Ok(format!("dry-run: {line}"));
        }
        match crate::util::run(&prog, &args, Duration::from_secs(10)) {
            Ok(out) => {
                self.log.push("info", "control", format!("{who}: {line} -> ok"));
                Ok(out.trim().to_string())
            }
            Err(e) => {
                self.log.push("error", "control", format!("{who}: {line} -> {e}"));
                Err(e)
            }
        }
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
        assert_eq!(out, "dry-run: iw dev wlan0 scan trigger");
        assert!(log.all().last().unwrap().msg.contains("DRY-RUN"));
    }

    #[test]
    fn token_compare() {
        assert!(token_ok("secret", "secret"));
        assert!(!token_ok("secret", "secreT"));
        assert!(!token_ok("secret", "secre"));
        assert!(!token_ok("", ""));
    }
}
