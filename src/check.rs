//! `--check`: a read-only self-test for the first run on real hardware.
//!
//! It verifies what the README lists as unverified (per-link MLO statistics,
//! neighbour widths, the hostapd `-l LINK` form) by comparing independent
//! sources, and prints - without running them - the exact command lines the
//! control actions would use, so they can be compared with `hostapd_cli -h`.
//! Nothing here changes radio or client state; only `status`, dumps and `-h`
//! are executed. The judging functions are pure and unit-tested.
use crate::collector::Collector;
use crate::control::{Action, Control, ControlCfg};
use crate::events::EventLog;
use crate::hostapd::{self, HostapdCfg};
use crate::ie::Width;
use crate::model::{Iface, State};
use crate::nl80211::{Nl, NlBss, NlStation};
use crate::sources::RealSource;
use crate::{debugfs, iwparse};
use std::collections::BTreeMap;
use std::time::Duration;

pub struct CheckCfg {
    pub iw: String,
    pub debugfs: String,
    pub hostapd_cli: String,
    pub hostapd_ctrl: Option<String>,
    pub netlink: bool,
    pub any_driver: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Level {
    Ok,
    Info,
    Warn,
    Fail,
}

#[derive(Default)]
pub struct Report {
    lines: Vec<(Level, String)>,
    sections: Vec<(usize, String)>,
}

impl Report {
    fn section(&mut self, title: &str) {
        self.sections.push((self.lines.len(), title.to_string()));
    }
    fn add(&mut self, l: Level, t: impl Into<String>) {
        self.lines.push((l, t.into()));
    }
    fn ok(&mut self, t: impl Into<String>) {
        self.add(Level::Ok, t)
    }
    fn info(&mut self, t: impl Into<String>) {
        self.add(Level::Info, t)
    }
    fn warn(&mut self, t: impl Into<String>) {
        self.add(Level::Warn, t)
    }
    fn fail(&mut self, t: impl Into<String>) {
        self.add(Level::Fail, t)
    }
    fn judged(&mut self, j: (Level, String)) {
        self.add(j.0, j.1)
    }
    pub fn count(&self, l: Level) -> usize {
        self.lines.iter().filter(|(x, _)| *x == l).count()
    }
    pub fn render(&self) -> String {
        let mut out = String::new();
        for (i, (l, t)) in self.lines.iter().enumerate() {
            for (_, title) in self.sections.iter().filter(|(at, _)| *at == i) {
                out += &format!("\n== {title} ==\n");
            }
            let tag = match l {
                Level::Ok => " OK ",
                Level::Info => "INFO",
                Level::Warn => "WARN",
                Level::Fail => "FAIL",
            };
            let mut it = t.lines();
            out += &format!("[{tag}] {}\n", it.next().unwrap_or(""));
            for cont in it {
                out += &format!("       {cont}\n");
            }
        }
        out
    }
}

// ------------------------------------------------------------ judgements --

/// Compare what the kernel reports per link (nl80211) with what mac80211's debugfs lists.
pub fn judge_station(mac: &str, debugfs_links: usize, nl: Option<&NlStation>) -> (Level, String) {
    let links = nl.map_or(&[][..], |s| &s.links[..]);
    let ids = links.iter().map(|l| l.id.to_string()).collect::<Vec<_>>().join(",");
    let filled = links
        .iter()
        .filter(|l| l.stats.signal.is_some() || l.stats.tx_bytes.is_some() || l.stats.rx_bytes.is_some())
        .count();
    if links.is_empty() && debugfs_links >= 2 {
        (
            Level::Fail,
            format!(
                "{mac}: MLO client (debugfs lists {debugfs_links} links) but nl80211 returned no per-link data\n\
                 run `iw dev IF station dump` and compare; try --no-netlink to see the iw-only view"
            ),
        )
    } else if links.len() >= 2 && filled >= 2 {
        if debugfs_links > links.len() {
            (Level::Warn, format!("{mac}: nl80211 reports links {ids}, debugfs lists {debugfs_links}"))
        } else {
            (Level::Ok, format!("{mac}: MLO client, per-link statistics for links {ids}"))
        }
    } else if links.len() >= 2 {
        (Level::Warn, format!("{mac}: links {ids} reported but without statistics"))
    } else if links.len() == 1 && debugfs_links >= 2 {
        (Level::Warn, format!("{mac}: debugfs lists {debugfs_links} links, nl80211 only link {ids}"))
    } else {
        (Level::Info, format!("{mac}: single-link client, nothing to verify"))
    }
}

/// Summarise the decoded neighbour widths.
pub fn judge_scan(iw_count: usize, nl: &[NlBss]) -> (Level, String) {
    if iw_count == 0 && nl.is_empty() {
        return (Level::Info, "scan cache is empty: the AP does not scan by itself, trigger a scan (control) or wait for ACS".into());
    }
    if nl.is_empty() {
        return (Level::Warn, format!("iw lists {iw_count} BSS but the nl80211 scan dump returned none: widths stay unknown"));
    }
    let mut hist: BTreeMap<u32, usize> = BTreeMap::new();
    let mut unknown = 0;
    for b in nl {
        match &b.width {
            Some(w) if w.center2.is_some() => *hist.entry(160).or_default() += 1,
            Some(w) => *hist.entry(w.mhz).or_default() += 1,
            None => unknown += 1,
        }
    }
    let h = hist.iter().map(|(w, n)| format!("{w} MHz: {n}")).collect::<Vec<_>>().join(", ");
    let what = format!("nl80211 scan dump: {} BSS (iw: {iw_count}); widths {h}{}", nl.len(), if unknown > 0 { format!(", unknown: {unknown}") } else { String::new() });
    if hist.is_empty() {
        (Level::Warn, format!("{what}\nno operation element could be decoded; compare with `iw dev IF scan dump`"))
    } else {
        (Level::Ok, what)
    }
}

/// `hostapd_cli status` for an interface / link against what `iw dev` says.
pub fn judge_hostapd(label: &str, expected_freq: Option<u32>, res: Result<BTreeMap<String, String>, String>) -> (Level, String) {
    match res {
        Err(e) => (Level::Warn, format!("{label}: {e}\ncheck --hostapd-ctrl DIR and that hostapd runs; DFS state and the control actions need it")),
        Ok(m) => {
            let freq: Option<u32> = m.get("freq").and_then(|f| f.parse().ok());
            let what = format!("{label}: state={} freq={} channel={}", m.get("state").map_or("?", |s| s), m.get("freq").map_or("?", |s| s), m.get("channel").map_or("?", |s| s));
            match (expected_freq, freq) {
                (Some(e), Some(f)) if e != f => (Level::Warn, format!("{what}, but iw reports {e} MHz for this link")),
                (_, None) => (Level::Warn, format!("{what} (no freq in the status output)")),
                _ => (Level::Ok, what),
            }
        }
    }
}

/// Does a `hostapd_cli -h` listing document option `-l`?
pub fn usage_has_link_option(usage: &str) -> bool {
    usage.lines().any(|l| l.trim_start().starts_with("-l") || l.contains("[-l"))
}

/// Does the listing document the command `name`?
pub fn usage_has_command(usage: &str, name: &str) -> bool {
    usage.lines().any(|l| l.trim_start().strip_prefix(name).is_some_and(|r| r.is_empty() || r.starts_with(' ')))
}

fn width_text(w: &Option<Width>) -> String {
    match w {
        None => "width unknown".into(),
        Some(w) => match w.center2 {
            Some(c2) => format!("80+80 MHz, centres {} / {c2}", w.center1),
            None if w.punct != 0 => format!("{} MHz, centre {}, punctured 0x{:x}", w.mhz, w.center1, w.punct),
            None => format!("{} MHz, centre {}", w.mhz, w.center1),
        },
    }
}

// -------------------------------------------------------------------- run --

/// stdout + stderr of a command, whatever its exit status (`hostapd_cli -h` exits non-zero).
fn output_any(prog: &str, args: &[&str]) -> Option<String> {
    let o = std::process::Command::new(prog).args(args).stdin(std::process::Stdio::null()).output().ok()?;
    Some(format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)))
}

fn ap_ifaces(ifaces: &[Iface]) -> Vec<&Iface> {
    ifaces.iter().filter(|i| matches!(i.kind.as_deref(), None | Some("AP") | Some("mesh point") | Some("P2P-GO"))).collect()
}

/// Run all checks and print the report. Returns the process exit code (1 if anything FAILed).
pub fn run(cfg: &CheckCfg) -> i32 {
    let mut r = Report::default();
    let mut facts = Facts::default();
    let timeout = Duration::from_secs(5);
    let iw = |args: &[&str]| crate::util::run_strs(&cfg.iw, args, timeout);

    // ---- environment
    r.section("Environment");
    if unsafe { libc::geteuid() } == 0 {
        r.ok("running as root");
    } else {
        r.warn("not running as root: debugfs panels and the kernel log are unreadable (the wireless panels still work)");
    }
    if let Some(k) = debugfs::read_text("/proc/sys/kernel/osrelease") {
        r.info(format!("kernel {}", k.trim()));
    }
    match iw(&["--version"]) {
        Ok(v) => r.ok(v.trim().to_string()),
        Err(e) => {
            r.fail(format!("cannot run iw: {e} (use --iw PATH)"));
            print!("{}", r.render());
            return 1;
        }
    }
    let mut phys = debugfs::find_phys();
    let ifaces: Vec<Iface> = match iw(&["dev"]) {
        Ok(t) => iwparse::parse_iw_dev(&t),
        Err(e) => {
            r.fail(e);
            vec![]
        }
    };
    if cfg.any_driver {
        phys = ifaces.iter().map(|i| format!("phy{}", i.phy)).collect();
        phys.sort();
        phys.dedup();
    }
    if phys.is_empty() {
        r.fail("no mt7996 phy found in /sys/class/ieee80211 (use --any-driver to check other drivers)");
    } else {
        r.ok(format!("phys: {}", phys.join(", ")));
    }
    for p in &phys {
        if debugfs::read_hw(p, &cfg.debugfs).readable {
            r.ok(format!("{p}: mt76 debugfs readable"));
        } else {
            r.warn(format!("{p}: mt76 debugfs not readable under {} (root? debugfs mounted?)", cfg.debugfs));
        }
    }

    // ---- interfaces
    r.section("Interfaces (iw dev)");
    let ifaces: Vec<Iface> = ifaces.into_iter().filter(|i| phys.contains(&format!("phy{}", i.phy))).collect();
    let aps = ap_ifaces(&ifaces);
    if aps.is_empty() {
        r.fail("no AP interface on these phys");
    }
    for i in &aps {
        if i.links.is_empty() {
            r.info(format!("{} (phy{}): single link, {} MHz ch {}", i.name, i.phy, i.base.width.unwrap_or(0), i.base.channel.unwrap_or(0)));
        } else {
            facts.has_mld = true;
            let l = i.links.iter().map(|(id, l)| format!("link {id}: ch {} {} MHz", l.channel.unwrap_or(0), l.width.unwrap_or(0))).collect::<Vec<_>>().join(", ");
            r.ok(format!("{} (phy{}): MLD with {} links ({l})", i.name, i.phy, i.links.len()));
        }
    }
    if !facts.has_mld && !aps.is_empty() {
        r.warn("no MLD AP found: the MLO checks below only see single-link interfaces");
    }

    // ---- per-link statistics of MLO clients
    r.section("MLO client statistics (nl80211 vs iw / debugfs)");
    let mut nl = None;
    if cfg.netlink {
        match Nl::open() {
            Ok(n) => {
                r.ok("nl80211 family resolved");
                nl = Some(n);
            }
            Err(e) => r.fail(format!("cannot open nl80211: {e}")),
        }
    } else {
        r.info("--no-netlink: skipped");
    }
    let mut stations = 0;
    for i in &aps {
        let Ok(t) = iw(&["dev", &i.name, "station", "dump"]) else { continue };
        let nl_list: Vec<NlStation> = match (nl.as_mut(), i.ifindex) {
            (Some(n), Some(idx)) => match n.dump_stations(idx) {
                Ok(l) => l,
                Err(e) => {
                    r.fail(format!("nl80211 station dump on {}: {e}", i.name));
                    vec![]
                }
            },
            _ => vec![],
        };
        for s in iwparse::parse_station_dump(&t) {
            stations += 1;
            let d = debugfs::read_sta_links(&format!("phy{}", i.phy), &i.name, &s.mac, &cfg.debugfs).len();
            let j = judge_station(&s.mac, d, nl_list.iter().find(|n| n.mac == s.mac));
            facts.mlo_verified |= j.0 == Level::Ok;
            r.judged(j);
        }
    }
    if stations == 0 {
        r.info("no client associated");
    }
    if cfg.netlink && !facts.mlo_verified {
        r.warn("per-link reader NOT verified: associate a Wi-Fi 7 client with MLO and run --check again");
    }

    // ---- neighbour widths
    r.section("Neighbour scan (planner input)");
    if aps.is_empty() {
        r.info("skipped: no AP interface");
    }
    for i in &aps {
        let iw_n = iw(&["dev", &i.name, "scan", "dump"]).map(|t| iwparse::parse_scan_dump(&t).len()).unwrap_or(0);
        let list = match (nl.as_mut(), i.ifindex) {
            (Some(n), Some(idx)) => n.dump_scan(idx).map_err(|e| r.warn(format!("nl80211 scan dump on {}: {e}", i.name))).unwrap_or_default(),
            _ => vec![],
        };
        if !cfg.netlink {
            r.info(format!("{}: iw lists {iw_n} BSS; widths need nl80211 (--no-netlink given)", i.name));
            continue;
        }
        let j = judge_scan(iw_n, &list);
        facts.widths_ok |= j.0 == Level::Ok;
        r.judged((j.0, format!("{}: {}", i.name, j.1)));
        // a few decoded BSSes, to compare by eye with `iw dev IF scan dump`
        for b in list.iter().filter(|b| b.width.is_some()).take(6) {
            r.info(format!("  {} freq {}: {}", b.bssid, b.freq, width_text(&b.width)));
        }
    }

    // ---- hostapd and the control commands
    r.section("hostapd_cli (read-only probes)");
    let usage = output_any(&cfg.hostapd_cli, &["-h"]);
    match &usage {
        None => r.warn(format!("cannot run {}: DFS state and all control actions are unavailable (use --hostapd-cli PATH)", cfg.hostapd_cli)),
        Some(u) => {
            r.info(u.lines().next().unwrap_or("").to_string());
            for cmd in ["chan_switch", "bss_tm_req"] {
                if usage_has_command(u, cmd) {
                    r.ok(format!("`{cmd}` is documented in hostapd_cli"));
                } else {
                    r.warn(format!("`{cmd}` not found in `hostapd_cli -h`: that control action will fail"));
                }
            }
            if usage_has_link_option(u) {
                r.ok("option -l <link_id> (MLD links) is supported");
            } else if facts.has_mld {
                r.fail("hostapd_cli has no -l <link_id> option but this AP is an MLD: chan_switch cannot address a link");
            } else {
                r.info("hostapd_cli has no -l <link_id> option (only needed for MLD APs)");
            }
        }
    }
    if usage.is_some() {
        let hcfg = HostapdCfg { cli: cfg.hostapd_cli.clone(), ctrl: cfg.hostapd_ctrl.clone() };
        for i in &aps {
            let targets: Vec<(Option<u32>, Option<u32>)> = if i.links.is_empty() {
                vec![(None, i.base.freq)]
            } else {
                i.links.iter().map(|(id, l)| (Some(*id), l.freq)).collect()
            };
            for (link, freq) in targets {
                let label = match link {
                    Some(l) => format!("{} -l {l} status", i.name),
                    None => format!("{} status", i.name),
                };
                let j = judge_hostapd(&label, freq, hostapd::status_link(&hcfg, &i.name, link));
                if link.is_some() && j.0 == Level::Ok {
                    facts.hostapd_link_ok = true;
                }
                r.judged(j);
            }
        }
    }
    for p in &phys {
        match iw(&["phy", p, "info"]) {
            Ok(t) if t.contains("preamble puncturing in AP mode") => r.ok(format!("{p}: kernel advertises preamble puncturing in AP mode")),
            Ok(_) => r.warn(format!("{p}: no 'preamble puncturing in AP mode' in `iw phy {p} info`: chan_switch with punct_bitmap will probably be refused")),
            Err(e) => r.warn(e),
        }
    }

    // ---- what the control actions would run
    r.section("Control commands (printed, not executed)");
    let ccfg = ControlCfg {
        enabled: true,
        iw: cfg.iw.clone(),
        hostapd_cli: cfg.hostapd_cli.clone(),
        hostapd_ctrl: cfg.hostapd_ctrl.clone(),
        ..Default::default()
    };
    let log = EventLog::new();
    let src = RealSource::new(cfg.iw.clone(), cfg.debugfs.clone(), cfg.any_driver, cfg.netlink, Some(HostapdCfg { cli: cfg.hostapd_cli.clone(), ctrl: cfg.hostapd_ctrl.clone() }));
    let col = Collector::new(Box::new(src), 1.0, Control::new(ControlCfg::default(), log), EventLog::new());
    col.poll_once();
    std::thread::sleep(Duration::from_secs(1));
    let st = col.poll_once();
    let pv = previews(&st);
    if pv.is_empty() {
        r.info("nothing to preview: no radio found");
    }
    for (tag, a) in pv {
        match a.argv(&ccfg) {
            Ok((p, args)) => r.info(format!("{tag}\n$ {p} {}", args.join(" "))),
            Err(e) => r.fail(format!("{tag}: {e}")),
        }
    }

    match output_any(&ccfg.ip, &["-V"]) {
        Some(_) => r.ok("`ip` (iproute2) found: the scan fallback for MLD APs can bring its temporary interface up"),
        None if facts.has_mld => r.warn("`ip` (iproute2) not found: scanning an MLD AP needs it for the temporary interface"),
        None => r.info("`ip` (iproute2) not found (only needed to scan an MLD AP)"),
    }
    r.info(format!(
        "scan on an MLD AP: the kernel refuses `scan trigger` there, so after that refusal the daemon runs\n\
         {iw} phy <phy> interface add {t} type managed; {ip} link set {t} up; {iw} dev {t} scan; {iw} dev {t} del\n\
         (the temporary interface is always removed again)",
        iw = ccfg.iw,
        ip = ccfg.ip,
        t = crate::control::TEMP_SCAN_IFACE
    ));

    // ---- summary
    r.section("Summary");
    r.info(format!("{} ok, {} warnings, {} failures", r.count(Level::Ok), r.count(Level::Warn), r.count(Level::Fail)));
    let mut todo: Vec<&str> = vec![];
    if cfg.netlink && !facts.mlo_verified {
        todo.push("per-link statistics of a real MLO client (associate one, run --check again)");
    }
    if cfg.netlink && !facts.widths_ok {
        todo.push("neighbour width decoding (needs scan results: trigger a scan, then --check again)");
    }
    if facts.has_mld && !facts.hostapd_link_ok {
        todo.push("hostapd_cli -l LINK against the running hostapd");
    }
    todo.push("chan_switch incl. punct_bitmap=, set_airtime, scan and btm: only a real run proves them. Try each once on a lab AP: start with --allow-control --control-dry-run, compare the lines above with `hostapd_cli -h`, then run one action from the UI and read the event log");
    for t in todo {
        r.info(format!("not verified by --check: {t}"));
    }
    print!("{}", r.render());
    if r.count(Level::Fail) > 0 {
        1
    } else {
        0
    }
}

#[derive(Default)]
struct Facts {
    has_mld: bool,
    mlo_verified: bool,
    widths_ok: bool,
    hostapd_link_ok: bool,
}

/// One example of each control action, built from the live state; the planner's
/// recommendation is used when there is one.
pub fn previews(st: &State) -> Vec<(String, Action)> {
    let mut v = vec![];
    if let Some(plan) = &st.plan {
        for l in &plan.links {
            let (what, freq, center1, punct) = if let Some(c) = &l.recommend {
                ("planner: switch channel", c.freq, c.center1, None)
            } else if let (Some(p), Some(c1)) = (&l.puncture, l.center1) {
                ("planner: puncture", l.freq, c1, Some(p.bitmap))
            } else {
                continue;
            };
            v.push((
                format!("chan_switch, {what} ({} {})", l.iface, l.band.unwrap_or("?")),
                Action::ChanSwitch { iface: l.iface.clone(), link: l.link, freq, center1, bandwidth: l.width, punct, count: None },
            ));
        }
    }
    if v.is_empty() {
        if let Some(r) = st.radios.iter().find(|r| r.freq.is_some() && r.center1.is_some() && r.width.is_some()) {
            v.push((
                format!("chan_switch, example: current channel of {} {}", r.iface, r.band.unwrap_or("?")),
                Action::ChanSwitch { iface: r.iface.clone(), link: r.link, freq: r.freq.unwrap(), center1: r.center1.unwrap(), bandwidth: r.width.unwrap(), punct: None, count: None },
            ));
        }
    }
    if let Some(s) = st.stations.first() {
        v.push(("set_airtime, example weight 128".into(), Action::SetAirtime { iface: s.iface.clone(), mac: s.mac.clone(), weight: 128 }));
    }
    if let Some(r) = st.radios.first() {
        v.push(("scan (the AP leaves its channel briefly)".into(), Action::Scan { iface: r.iface.clone() }));
    }
    // btm only when the steering advisor currently has a suggestion (needs a single-link client on a congested link)
    for a in st.steering.iter().take(1) {
        v.push((
            format!("btm, current steering advice for {}", a.mac),
            Action::Btm { iface: st.stations.iter().find(|s| s.mac == a.mac).map_or_else(|| "wlan0".into(), |s| s.iface.clone()), mac: a.mac.clone(), neighbor: a.neighbor.clone() },
        ));
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Stats;
    use crate::nl80211::NlLink;

    fn link(id: u32, signal: Option<i32>) -> NlLink {
        NlLink { id, stats: Stats { signal, ..Default::default() } }
    }
    fn nls(links: Vec<NlLink>) -> NlStation {
        NlStation { mac: "aa".into(), stats: None, links }
    }

    #[test]
    fn station_verdicts() {
        let mac = "10:91:d1:f7:5e:bd";
        // working MLO reader
        let two = nls(vec![link(0, Some(-50)), link(1, Some(-60))]);
        assert_eq!(judge_station(mac, 2, Some(&two)).0, Level::Ok);
        // debugfs knows the MLO client, nl80211 returned nothing: the per-link reader is broken
        let j = judge_station(mac, 2, Some(&nls(vec![])));
        assert_eq!(j.0, Level::Fail);
        assert!(j.1.contains("--no-netlink"));
        assert_eq!(judge_station(mac, 2, None).0, Level::Fail);
        // link ids without numbers, or fewer links than debugfs
        assert_eq!(judge_station(mac, 2, Some(&nls(vec![link(0, None), link(1, None)]))).0, Level::Warn);
        assert_eq!(judge_station(mac, 2, Some(&nls(vec![link(1, Some(-50))]))).0, Level::Warn);
        assert_eq!(judge_station(mac, 3, Some(&two)).0, Level::Warn);
        // single-link client, debugfs unreadable: nothing to say
        assert_eq!(judge_station(mac, 0, Some(&nls(vec![link(0, Some(-50))]))).0, Level::Info);
        assert_eq!(judge_station(mac, 0, None).0, Level::Info);
        // MLO per nl80211 even without debugfs
        assert_eq!(judge_station(mac, 0, Some(&two)).0, Level::Ok);
    }

    #[test]
    fn scan_verdicts() {
        let w = |mhz| Some(Width { mhz, center1: 5210, center2: None, punct: 0 });
        let b = |width| NlBss { bssid: "aa".into(), freq: 5180, width };
        assert_eq!(judge_scan(0, &[]).0, Level::Info);
        assert_eq!(judge_scan(5, &[]).0, Level::Warn);
        let j = judge_scan(3, &[b(w(80)), b(w(80)), b(w(20)), b(None)]);
        assert_eq!(j.0, Level::Ok);
        assert!(j.1.contains("20 MHz: 1, 80 MHz: 2") && j.1.contains("unknown: 1"), "{}", j.1);
        // nothing decodable at all
        assert_eq!(judge_scan(2, &[b(None), b(None)]).0, Level::Warn);
    }

    #[test]
    fn hostapd_verdicts() {
        let m = |s: &str| Ok(crate::hostapd::parse_status(s));
        assert_eq!(judge_hostapd("wlan0", Some(5500), m("state=ENABLED\nfreq=5500\nchannel=100\n")).0, Level::Ok);
        assert_eq!(judge_hostapd("wlan0", Some(5180), m("state=ENABLED\nfreq=5500\nchannel=100\n")).0, Level::Warn);
        assert_eq!(judge_hostapd("wlan0", None, m("state=ENABLED\nfreq=5500\n")).0, Level::Ok);
        assert_eq!(judge_hostapd("wlan0", Some(5500), m("state=DISABLED\n")).0, Level::Warn);
        let j = judge_hostapd("wlan0 -l 1", Some(5500), Err("hostapd_cli: connect failed".into()));
        assert_eq!(j.0, Level::Warn);
        assert!(j.1.contains("--hostapd-ctrl"));
    }

    // Trimmed from hostapd_cli v2.12-devel -h
    const USAGE: &str = "hostapd_cli v2.12-devel\n\nusage: hostapd_cli [-p<path>] [-i<ifname>] [-l<link_id>] [-hvBr]\n\nOptions:\n   -h           help\n   -l<link_id>  Link ID of the interface in case of Multi-Link Operation\n\ncommands:\n  status = show interface status info\n  chan_switch <cs_count> <freq> [sec_channel_offset=]\n  bss_tm_req = send BSS Transition Management Request\n";

    #[test]
    fn usage_parsing() {
        assert!(usage_has_link_option(USAGE));
        // only in the "usage:" line, or not at all
        assert!(usage_has_link_option("usage: hostapd_cli [-p<path>] [-l<link_id>] [-hvBr]\n"));
        assert!(!usage_has_link_option("usage: hostapd_cli [-p<path>] [-hvBr]\n   -p<path>  x\n"));
        assert!(usage_has_command(USAGE, "chan_switch"));
        assert!(usage_has_command(USAGE, "bss_tm_req"));
        assert!(!usage_has_command(USAGE, "chan_switch_all"));
        assert!(!usage_has_command(USAGE, "stat"));
    }

    #[test]
    fn report_rendering_and_counts() {
        let mut r = Report::default();
        r.section("A");
        r.ok("fine");
        r.warn("two\nlines");
        r.section("B");
        r.fail("bad");
        let t = r.render();
        assert_eq!(t, "\n== A ==\n[ OK ] fine\n[WARN] two\n       lines\n\n== B ==\n[FAIL] bad\n");
        assert_eq!((r.count(Level::Ok), r.count(Level::Warn), r.count(Level::Fail)), (1, 1, 1));
    }

    #[test]
    fn previews_come_from_the_plan_and_validate() {
        // the demo AP has recommendations, a client and radios
        let log = EventLog::new();
        let col = Collector::new(Box::new(crate::sources::DemoSource::new()), 1.0, Control::new(ControlCfg::default(), log.clone()), log);
        col.poll_once();
        let st = col.poll_once();
        let p = previews(&st);
        assert!(p.iter().any(|(t, a)| t.starts_with("chan_switch") && matches!(a, Action::ChanSwitch { .. })));
        assert!(p.iter().any(|(_, a)| matches!(a, Action::SetAirtime { .. })));
        assert!(p.iter().any(|(_, a)| matches!(a, Action::Scan { .. })));
        let cfg = ControlCfg { enabled: true, ..Default::default() };
        for (tag, a) in &p {
            assert!(a.argv(&cfg).is_ok(), "{tag}");
        }
    }
}
