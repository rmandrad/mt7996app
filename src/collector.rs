//! Polls a Source, derives rates from counters, keeps a short history and
//! runs the planner / airtime / steering logic on every sample.
use crate::airtime::{AirtimeAdvice, AirtimeCfg, AirtimeCtl};
use crate::control::{Action, Control};
use crate::events::EventLog;
use crate::iwparse::band_of;
use crate::model::*;
use crate::planner::{build_plan, Plan, PlanInput};
use crate::steering::{SteerAdvice, SteerCfg, SteerCtl};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const HISTORY_LEN: usize = 300; // samples per series
const STALE_SECS: f64 = 600.0; // drop series/counters that stopped updating
const PLAN_EVERY_SECS: f64 = 10.0;

/// Counters that rates are derived from.
#[derive(Clone, Copy, Default)]
struct Ctr {
    tx_bytes: Option<u64>,
    rx_bytes: Option<u64>,
    tx_pkts: Option<u64>,
    tx_retries: Option<u64>,
}

impl Ctr {
    fn of(s: &Stats) -> Self {
        Self { tx_bytes: s.tx_bytes, rx_bytes: s.rx_bytes, tx_pkts: s.tx_pkts, tx_retries: s.tx_retries }
    }
}

/// Counter delta; `None` on a reset (reassociation) or a missing value.
fn delta(cur: Option<u64>, prev: Option<u64>) -> Option<u64> {
    cur?.checked_sub(prev?)
}

fn round(x: f64, places: i32) -> f64 {
    let m = 10f64.powi(places);
    (x * m).round() / m
}

fn derive(s: &mut Stats, prev: Option<Ctr>, dt: f64) {
    s.mbps_tx = None;
    s.mbps_rx = None;
    s.retry_pct = None;
    let Some(p) = prev.filter(|_| dt > 0.0) else { return };
    let c = Ctr::of(s);
    s.mbps_tx = delta(c.tx_bytes, p.tx_bytes).map(|d| round(d as f64 * 8.0 / dt / 1e6, 2));
    s.mbps_rx = delta(c.rx_bytes, p.rx_bytes).map(|d| round(d as f64 * 8.0 / dt / 1e6, 2));
    if let (Some(r), Some(k)) = (delta(c.tx_retries, p.tx_retries), delta(c.tx_pkts, p.tx_pkts)) {
        s.retry_pct = Some(if r + k == 0 { 0.0 } else { round((100.0 * r as f64 / (r + k) as f64).min(100.0), 1) });
    }
}

pub fn now_ts() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// What the previous sample looked like, to log changes.
#[derive(Default)]
struct View {
    radios: HashMap<(String, Option<u32>), (Option<u32>, Option<u32>, Option<u32>)>, // freq, width, channel
    stations: HashMap<String, (String, BTreeSet<u32>)>,
    rdd: HashMap<String, Option<String>>,
    hostapd: HashMap<String, BTreeMap<String, String>>,
    errors: HashSet<String>,
    started: bool,
}

#[derive(Default)]
struct Inner {
    state_json: Arc<String>,
    history: BTreeMap<String, VecDeque<(f64, f64)>>,
    last_seen: HashMap<String, f64>,
    prev: HashMap<String, (f64, Ctr)>,
    prev_survey: HashMap<String, SurveyChan>,
    scan: Option<ScanData>,
    phy_chans: HashMap<String, Vec<ChanInfo>>,
    survey: HashMap<String, Vec<SurveyChan>>,
    plan: Option<Plan>,
    last_plan: f64,
    airtime: AirtimeCtl,
    steer: SteerCtl,
    view: View,
}

pub struct Collector {
    source: Mutex<Box<dyn Source>>,
    pub interval: f64,
    host: String,
    demo: bool,
    pub ctl: Arc<Control>,
    log: Arc<EventLog>,
    inner: Mutex<Inner>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Collector {
    pub fn new(source: Box<dyn Source>, interval: f64, ctl: Arc<Control>, log: Arc<EventLog>) -> Arc<Self> {
        let host = std::fs::read_to_string("/proc/sys/kernel/hostname")
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| "unknown".into());
        let demo = source.demo();
        let inner = Inner {
            airtime: AirtimeCtl::new(AirtimeCfg::default(), ctl.cfg.enabled && ctl.cfg.airtime_auto),
            steer: SteerCtl::new(SteerCfg::default()),
            ..Default::default()
        };
        let c = Arc::new(Self { source: Mutex::new(source), interval, host, demo, ctl, log, inner: Mutex::new(inner) });
        let waiting = State {
            host: c.host.clone(),
            demo,
            interval,
            errors: vec!["waiting for first sample".into()],
            control: c.control_info(),
            ..Default::default()
        };
        lock(&c.inner).state_json = Arc::new(serde_json::to_string(&waiting).unwrap());
        c
    }

    fn control_info(&self) -> ControlInfo {
        let c = &self.ctl.cfg;
        ControlInfo { enabled: c.enabled, dry_run: c.dry_run, airtime_auto: c.enabled && c.airtime_auto, steer_auto: c.enabled && c.steer_auto }
    }

    pub fn state_json(&self) -> Arc<String> {
        lock(&self.inner).state_json.clone()
    }

    pub fn history_json(&self) -> String {
        serde_json::to_string(&lock(&self.inner).history).unwrap_or_else(|_| "{}".into())
    }

    pub fn start(self: &Arc<Self>) -> std::thread::JoinHandle<()> {
        let me = self.clone();
        std::thread::Builder::new()
            .name("collector".into())
            .spawn(move || loop {
                let t0 = Instant::now();
                me.poll_once();
                let wait = Duration::from_secs_f64(me.interval).saturating_sub(t0.elapsed());
                std::thread::sleep(wait.max(Duration::from_millis(200)));
            })
            .expect("spawn collector")
    }

    pub fn poll_once(&self) -> State {
        // A parser bug must not take the daemon down.
        let raw = catch_unwind(AssertUnwindSafe(|| lock(&self.source).raw())).unwrap_or_else(|_| Raw {
            errors: vec!["collector error: source panicked".into()],
            ..Default::default()
        });
        let (state, auto, steered) = {
            let mut inner = lock(&self.inner);
            let (state, auto, steered) = self.process(&mut inner, raw, now_ts());
            inner.state_json = Arc::new(serde_json::to_string(&state).unwrap_or_else(|_| "{}".into()));
            (state, auto, steered)
        };
        // Auto-mode actions run after the lock is released: they spawn processes.
        for (who, a) in &auto {
            let _ = self.ctl.execute(who, a);
        }
        if !steered.is_empty() {
            let mut inner = lock(&self.inner);
            for m in steered {
                inner.steer.mark_sent(&m, now_ts());
            }
        }
        state
    }

    fn process(&self, inner: &mut Inner, raw: Raw, now: f64) -> (State, Vec<(&'static str, Action)>, Vec<String>) {
        let Raw { scan, phy_chans, hostapd, ifaces, stations: mut sta_list, survey, hw, errors } = raw;
        if let Some(s) = scan {
            inner.scan = Some(s);
        }
        if let Some(p) = phy_chans {
            inner.phy_chans = p;
        }
        inner.survey = survey.clone();

        let mut link_info: HashMap<(String, u32), IfLink> = HashMap::new();
        for i in &ifaces {
            for (lid, l) in &i.links {
                link_info.insert((i.name.clone(), *lid), l.clone());
            }
        }

        for s in &mut sta_list {
            let key = format!("sta|{}|{}", s.iface, s.mac);
            let prev = inner.prev.get(&key).map(|p| (p.0, p.1));
            derive(&mut s.stats, prev.map(|p| p.1), prev.map_or(0.0, |p| now - p.0));
            inner.prev.insert(key, (now, Ctr::of(&s.stats)));

            let sid = format!("sta:{}", s.mac);
            push(inner, format!("{sid}:tx"), now, s.stats.mbps_tx);
            push(inner, format!("{sid}:rx"), now, s.stats.mbps_rx);
            push(inner, format!("{sid}:sig"), now, s.stats.signal.map(f64::from));

            for l in &mut s.links {
                let lkey = format!("sta|{}|{}|{}", s.iface, s.mac, l.id);
                let prev = inner.prev.get(&lkey).map(|p| (p.0, p.1));
                derive(&mut l.stats, prev.map(|p| p.1), prev.map_or(0.0, |p| now - p.0));
                inner.prev.insert(lkey, (now, Ctr::of(&l.stats)));
                if let Some(info) = link_info.get(&(s.iface.clone(), l.id)) {
                    l.freq = info.freq;
                    l.band = band_of(info.freq);
                    l.channel = info.channel;
                }
                push(inner, format!("{sid}:l{}:tx", l.id), now, l.stats.mbps_tx);
                push(inner, format!("{sid}:l{}:sig", l.id), now, l.stats.signal.map(f64::from));
            }
        }

        let mut radios = radios(inner, &ifaces, &survey, now);

        // SNR = signal - noise floor of the link the signal was measured on
        let noise_of = |iface: &str, link: Option<u32>| radios.iter().find(|r| r.iface == iface && r.link == link).and_then(|r| r.noise);
        for s in &mut sta_list {
            for l in &mut s.links {
                l.stats.snr = l.stats.signal.zip(noise_of(&s.iface, Some(l.id))).map(|(a, b)| a - b);
                if let Some(snr) = l.stats.snr {
                    let key = format!("sta:{}:l{}:snr", s.mac, l.id);
                    push(inner, key, now, Some(snr as f64));
                }
            }
            s.stats.snr = if s.links.is_empty() {
                let rs: Vec<&Radio> = radios.iter().filter(|r| r.iface == s.iface).collect();
                if rs.len() == 1 { s.stats.signal.zip(rs[0].noise).map(|(a, b)| a - b) } else { None }
            } else {
                s.links.iter().filter_map(|l| l.stats.snr).max()
            };
            push(inner, format!("sta:{}:snr", s.mac), now, s.stats.snr.map(f64::from));
        }

        let mut counts: HashMap<(String, Option<u32>), usize> = HashMap::new();
        for s in &sta_list {
            if s.links.is_empty() {
                *counts.entry((s.iface.clone(), None)).or_default() += 1;
            }
            for l in &s.links {
                *counts.entry((s.iface.clone(), Some(l.id))).or_default() += 1;
            }
        }
        for r in &mut radios {
            let unlinked = counts.get(&(r.iface.clone(), None)).copied().unwrap_or(0);
            if r.link.is_some() {
                r.stations = counts.get(&(r.iface.clone(), r.link)).copied().unwrap_or(0);
                r.stations_unlinked = unlinked;
            } else {
                r.stations = unlinked;
            }
        }

        self.diff_events(&mut inner.view, &radios, &sta_list, &hw, &hostapd, &errors);

        // planner (advisory, cheap, but not needed on every sample)
        if now - inner.last_plan >= PLAN_EVERY_SECS {
            inner.last_plan = now;
            let p = build_plan(&PlanInput { now, radios: &radios, scan: inner.scan.as_ref(), survey: &inner.survey, phy_chans: &inner.phy_chans });
            inner.plan = Some(p);
        }

        // airtime fairness
        let (airtime, airtime_actions): (Vec<AirtimeAdvice>, Vec<Action>) = inner.airtime.step(now, &sta_list);
        let mut auto: Vec<(&'static str, Action)> = airtime_actions.into_iter().map(|a| ("auto-airtime", a)).collect();

        // band steering
        let steer = inner.steer.advise(now, &sta_list, &radios);
        let mut steered = vec![];
        if self.ctl.cfg.enabled && self.ctl.cfg.steer_auto {
            for (a, act) in &steer {
                if a.cooldown_left_s <= 0.0 {
                    steered.push(a.mac.clone());
                    auto.push(("auto-steer", act.clone()));
                }
            }
        }
        let steering: Vec<SteerAdvice> = steer.into_iter().map(|(a, _)| a).collect();

        gc(inner, now);
        let state = State {
            ts: now,
            host: self.host.clone(),
            demo: self.demo,
            interval: self.interval,
            errors,
            radios,
            stations: sta_list,
            hw,
            plan: inner.plan.clone(),
            airtime,
            steering,
            control: self.control_info(),
        };
        (state, auto, steered)
    }

    /// Log what changed since the previous sample.
    fn diff_events(
        &self,
        v: &mut View,
        radios: &[Radio],
        stations: &[Station],
        hw: &[Hw],
        hostapd: &HashMap<String, BTreeMap<String, String>>,
        errors: &[String],
    ) {
        let log = &self.log;
        let first = !v.started;
        v.started = true;

        let mut seen = HashSet::new();
        for r in radios {
            let key = (r.iface.clone(), r.link);
            let cur = (r.freq, r.width, r.channel);
            let name = format!("{}{}", r.iface, r.link.map_or(String::new(), |l| format!(" link {l}")));
            seen.insert(key.clone());
            match v.radios.insert(key, cur) {
                Some(old) if old != cur && !first => log.push(
                    "warn",
                    "channel",
                    format!("{name}: channel {} ({} MHz) -> {} ({} MHz)", opt(old.2), opt(old.1), opt(cur.2), opt(cur.1)),
                ),
                None if !first => log.push("info", "channel", format!("{name}: appeared on channel {} ({} MHz)", opt(cur.2), opt(cur.1))),
                _ => {}
            }
        }
        let gone: Vec<_> = v.radios.keys().filter(|k| !seen.contains(*k)).cloned().collect();
        for k in gone {
            v.radios.remove(&k);
            log.push("warn", "channel", format!("{}{}: radio disappeared", k.0, k.1.map_or(String::new(), |l| format!(" link {l}"))));
        }

        let mut present = HashSet::new();
        for s in stations {
            let links: BTreeSet<u32> = s.links.iter().map(|l| l.id).collect();
            present.insert(s.mac.clone());
            let desc = if s.mlo { format!("MLO, links {links:?}") } else { "single link".to_string() };
            match v.stations.insert(s.mac.clone(), (s.iface.clone(), links.clone())) {
                None if !first => log.push("info", "station", format!("{} joined {} ({desc})", s.mac, s.iface)),
                Some((_, old)) if old != links && !first => {
                    log.push("info", "link", format!("{}: link set {old:?} -> {links:?}", s.mac))
                }
                _ => {}
            }
        }
        let left: Vec<String> = v.stations.keys().filter(|m| !present.contains(*m)).cloned().collect();
        for m in left {
            v.stations.remove(&m);
            log.push("info", "station", format!("{m} left"));
        }

        for h in hw {
            let cur = h.rdd.clone();
            if let Some(old) = v.rdd.insert(h.phy.clone(), cur.clone()) {
                if old != cur && !first {
                    log.push("info", "dfs", format!("{}: rdd_monitor: {}", h.phy, cur.unwrap_or_else(|| "(unavailable)".into())));
                }
            }
        }

        for (iface, st) in hostapd {
            let (old_state, new_state) = (v.hostapd.get(iface).and_then(|m| m.get("state")).cloned(), st.get("state").cloned());
            if old_state != new_state && !first {
                let left = st.get("cac_time_left_seconds").map(|l| format!(" (CAC {l} s left)")).unwrap_or_default();
                let dfs = old_state.as_deref() == Some("DFS") || new_state.as_deref() == Some("DFS");
                log.push(
                    if dfs { "warn" } else { "info" },
                    if dfs { "dfs" } else { "system" },
                    format!("hostapd {iface}: state {} -> {}{left}", old_state.unwrap_or_default(), new_state.unwrap_or_default()),
                );
            }
            v.hostapd.insert(iface.clone(), st.clone());
        }

        for e in errors {
            if v.errors.insert(e.clone()) {
                log.push("warn", "system", e.clone());
            }
        }
        v.errors.retain(|e| errors.contains(e));

        if first {
            log.push("info", "system", format!("monitoring started: {} radio(s), {} station(s)", radios.len(), stations.len()));
        }
    }
}

fn opt<T: std::fmt::Display>(v: Option<T>) -> String {
    v.map_or("?".into(), |x| x.to_string())
}

fn push(inner: &mut Inner, key: String, ts: f64, val: Option<f64>) {
    let Some(v) = val else { return };
    let h = inner.history.entry(key.clone()).or_default();
    if h.len() >= HISTORY_LEN {
        h.pop_front();
    }
    h.push_back((round(ts, 1), v));
    inner.last_seen.insert(key, ts);
}

fn radios(inner: &mut Inner, ifaces: &[Iface], survey: &HashMap<String, Vec<SurveyChan>>, now: f64) -> Vec<Radio> {
    let mut out = vec![];
    for i in ifaces {
        let entries: Vec<(Option<u32>, &IfLink)> = if i.links.is_empty() {
            vec![(None, &i.base)]
        } else {
            i.links.iter().map(|(k, v)| (Some(*k), v)).collect()
        };
        for (lid, l) in entries {
            let freq = l.freq.or(i.base.freq);
            let mut r = Radio {
                iface: i.name.clone(),
                phy: Some(format!("phy{}", i.phy)),
                link: lid,
                ssid: i.ssid.clone(),
                kind: i.kind.clone(),
                freq,
                band: band_of(freq),
                channel: l.channel.or(i.base.channel),
                width: l.width.or(i.base.width),
                center1: l.center1.or(i.base.center1),
                txpower: l.txpower.or(i.base.txpower),
                addr: l.addr.clone().or_else(|| i.base.addr.clone()),
                ..Default::default()
            };
            let chans = survey.get(&i.name).map(Vec::as_slice).unwrap_or(&[]);
            if let Some(c) = chans.iter().find(|c| Some(c.freq) == freq && c.in_use) {
                r.noise = c.noise;
                let pkey = format!("svy|{}|{}", i.name, c.freq);
                if let Some(p) = inner.prev_survey.get(&pkey) {
                    if let (Some(a), Some(pa)) = (c.active_ms, p.active_ms) {
                        if a > pa {
                            let da = (a - pa) as f64;
                            let pct = |c: Option<u64>, p: Option<u64>| delta(c, p).map(|d| round(100.0 * d as f64 / da, 1));
                            r.busy_pct = pct(c.busy_ms, p.busy_ms);
                            r.tx_pct = pct(c.tx_ms, p.tx_ms);
                            r.rx_pct = pct(c.rx_ms, p.rx_ms);
                        }
                    }
                }
                inner.prev_survey.insert(pkey, c.clone());
                let hk = format!("chan:{}:{}:busy", i.name, c.freq);
                push(inner, hk.clone(), now, r.busy_pct);
                r.history_key = Some(hk);
            }
            out.push(r);
        }
    }
    out
}

fn gc(inner: &mut Inner, now: f64) {
    let stale: Vec<String> = inner.last_seen.iter().filter(|(_, t)| now - **t > STALE_SECS).map(|(k, _)| k.clone()).collect();
    for k in stale {
        inner.history.remove(&k);
        inner.last_seen.remove(&k);
    }
    inner.prev.retain(|_, (t, _)| now - *t <= STALE_SECS);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::ControlCfg;
    use crate::sources::DemoSource;

    fn collector(cfg: ControlCfg) -> (Arc<Collector>, Arc<EventLog>) {
        let log = EventLog::new();
        let ctl = Control::new(cfg, log.clone());
        (Collector::new(Box::new(DemoSource::new()), 2.0, ctl, log.clone()), log)
    }

    #[test]
    fn derive_rates_and_reset() {
        let mut s = Stats { tx_bytes: Some(3_000_000), rx_bytes: Some(1_000_000), tx_pkts: Some(1900), tx_retries: Some(100), ..Default::default() };
        let prev = Ctr { tx_bytes: Some(1_000_000), rx_bytes: Some(500_000), tx_pkts: Some(1000), tx_retries: Some(0) };
        derive(&mut s, Some(prev), 2.0);
        assert_eq!(s.mbps_tx, Some(8.0));
        assert_eq!(s.mbps_rx, Some(2.0));
        assert_eq!(s.retry_pct, Some(10.0)); // 100 / (900 + 100)
        // counter went backwards (reassociation): no rate, not a huge number
        let prev = Ctr { tx_bytes: Some(9_000_000), ..prev };
        derive(&mut s, Some(prev), 2.0);
        assert_eq!(s.mbps_tx, None);
        // first sample has no previous
        derive(&mut s, None, 0.0);
        assert_eq!((s.mbps_tx, s.retry_pct), (None, None));
    }

    #[test]
    fn demo_pipeline_produces_everything() {
        let (c, log) = collector(ControlCfg::default());
        let mut inner = Inner::default();
        inner.airtime = AirtimeCtl::new(AirtimeCfg::default(), false);
        let raw = |c: &Collector| lock(&c.source).raw();
        let (s1, _, _) = c.process(&mut inner, raw(&c), 100.0);
        assert!(s1.stations[0].stats.mbps_tx.is_none());
        let (s2, auto, _) = c.process(&mut inner, raw(&c), 102.0);
        assert!(auto.is_empty()); // control disabled
        let mlo = &s2.stations[0];
        assert!(mlo.mlo && mlo.links.len() == 3);
        assert!(mlo.stats.mbps_tx.unwrap() > 100.0);
        assert!(mlo.links[2].stats.mbps_tx.unwrap() > mlo.links[0].stats.mbps_tx.unwrap());
        assert_eq!(mlo.links[1].band, Some("5G"));
        assert!(mlo.links.iter().all(|l| l.stats.snr.is_some()) && mlo.stats.snr.is_some());
        assert_eq!(s2.radios.len(), 3);
        assert!(s2.radios.iter().all(|r| r.busy_pct.is_some() && r.stations >= 1));
        assert!(inner.history.contains_key("sta:a4:83:e7:10:20:30:tx"));
        // planner ran on the first sample and has all three links
        let plan = s2.plan.as_ref().unwrap();
        assert_eq!(plan.links.len(), 3);
        // the demo's 5 GHz link has a busy secondary subchannel -> puncturing advice
        assert!(plan.links.iter().any(|l| l.puncture.is_some()), "{:?}", plan.links.iter().map(|l| (&l.puncture_note, &l.bins)).collect::<Vec<_>>());
        // the Wi-Fi 6 phone on the congested 2.4 GHz link is a steering candidate, the MLO phone is not
        assert_eq!(s2.steering.len(), 1, "{:?}", s2.steering);
        assert_eq!((s2.steering[0].mac.as_str(), s2.steering[0].to_band), ("d0:81:7a:55:66:77", Some("5G")));
        // airtime advice exists and flags the chatty IoT client
        assert!(!s2.airtime.is_empty());
        assert!(s2.airtime.iter().any(|a| a.mac == "b8:27:eb:01:02:03" && a.hog), "{:?}", s2.airtime);
        // state is valid JSON
        serde_json::from_str::<serde_json::Value>(&serde_json::to_string(&s2).unwrap()).unwrap();
        let msgs: Vec<String> = log.all().into_iter().map(|e| e.msg).collect();
        assert!(msgs.iter().any(|m| m.starts_with("monitoring started")));
    }

    #[test]
    fn events_for_changes() {
        let (c, log) = collector(ControlCfg::default());
        let mut v = View::default();
        let radio = |ch| Radio { iface: "wlan0".into(), link: Some(1), freq: Some(5180), width: Some(80), channel: Some(ch), ..Default::default() };
        let sta = |links: &[u32]| Station {
            mac: "aa:bb:cc:dd:ee:ff".into(),
            iface: "wlan0".into(),
            mlo: links.len() > 1,
            links: links.iter().map(|id| Link { id: *id, ..Default::default() }).collect(),
            ..Default::default()
        };
        let none = HashMap::new();
        c.diff_events(&mut v, &[radio(36)], &[], &[], &none, &[]);
        c.diff_events(&mut v, &[radio(36)], &[sta(&[0, 1])], &[], &none, &[]);
        c.diff_events(&mut v, &[radio(100)], &[sta(&[1])], &[], &none, &["boom".to_string()]);
        let hs = HashMap::from([("wlan0".to_string(), BTreeMap::from([("state".to_string(), "DFS".to_string())]))]);
        c.diff_events(&mut v, &[radio(100)], &[], &[], &hs, &["boom".to_string()]);
        let hs2 = HashMap::from([("wlan0".to_string(), BTreeMap::from([("state".to_string(), "ENABLED".to_string())]))]);
        c.diff_events(&mut v, &[radio(100)], &[], &[], &hs2, &[]);
        let ev = log.all();
        let has = |k: &str, needle: &str| ev.iter().any(|e| e.kind == k && e.msg.contains(needle));
        assert!(has("system", "monitoring started"));
        assert!(has("station", "joined wlan0 (MLO"));
        assert!(has("link", "{0, 1} -> {1}"));
        assert!(has("channel", "channel 36 (80 MHz) -> 100 (80 MHz)"));
        assert!(has("station", "left"));
        assert!(has("system", "boom"));
        assert_eq!(ev.iter().filter(|e| e.msg == "boom").count(), 1); // not repeated
        assert!(has("dfs", "state ENABLED -> DFS") || has("dfs", "state  -> DFS"), "{ev:?}");
        assert!(has("dfs", "DFS -> ENABLED"));
    }
}
