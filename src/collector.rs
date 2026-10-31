//! Polls a Source, derives rates from counters and keeps a short history.
use crate::iwparse::band_of;
use crate::model::*;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const HISTORY_LEN: usize = 300; // samples per series
const STALE_SECS: f64 = 600.0; // drop series/counters that stopped updating

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

#[derive(Default)]
struct Inner {
    state_json: Arc<String>,
    history: BTreeMap<String, VecDeque<(f64, f64)>>,
    last_seen: HashMap<String, f64>,
    prev: HashMap<String, (f64, Ctr)>,
    prev_survey: HashMap<String, SurveyChan>,
}

pub struct Collector {
    source: Mutex<Box<dyn Source>>,
    pub interval: f64,
    host: String,
    demo: bool,
    inner: Mutex<Inner>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Collector {
    pub fn new(source: Box<dyn Source>, interval: f64) -> Arc<Self> {
        let host = std::fs::read_to_string("/proc/sys/kernel/hostname")
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| "unknown".into());
        let demo = source.demo();
        let c = Arc::new(Self {
            source: Mutex::new(source),
            interval,
            host,
            demo,
            inner: Mutex::new(Inner::default()),
        });
        let waiting = State {
            host: c.host.clone(),
            demo,
            interval,
            errors: vec!["waiting for first sample".into()],
            ..Default::default()
        };
        lock(&c.inner).state_json = Arc::new(serde_json::to_string(&waiting).unwrap());
        c
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
            .spawn(move || {
                loop {
                    let t0 = Instant::now();
                    me.poll_once();
                    let wait = Duration::from_secs_f64(me.interval).saturating_sub(t0.elapsed());
                    std::thread::sleep(wait.max(Duration::from_millis(200)));
                }
            })
            .expect("spawn collector")
    }

    pub fn poll_once(&self) -> State {
        // A parser bug must not take the daemon down.
        let raw = catch_unwind(AssertUnwindSafe(|| lock(&self.source).raw())).unwrap_or_else(|_| Raw {
            errors: vec!["collector error: source panicked".into()],
            ..Default::default()
        });
        let mut inner = lock(&self.inner);
        let state = self.process(&mut inner, raw, now_ts());
        inner.state_json = Arc::new(serde_json::to_string(&state).unwrap_or_else(|_| "{}".into()));
        state
    }

    fn process(&self, inner: &mut Inner, raw: Raw, now: f64) -> State {
        let Raw { ifaces, stations: mut sta_list, survey, hw, errors } = raw;

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
            r.stations = counts
                .get(&(r.iface.clone(), r.link))
                .or_else(|| counts.get(&(r.iface.clone(), None)))
                .copied()
                .unwrap_or(0);
        }

        gc(inner, now);
        State { ts: now, host: self.host.clone(), demo: self.demo, interval: self.interval, errors, radios, stations: sta_list, hw }
    }
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
    use crate::sources::DemoSource;

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
    fn demo_two_polls_produce_rates() {
        let c = Collector::new(Box::new(DemoSource::new()), 2.0);
        let mut inner = Inner::default();
        let raw = |c: &Collector| lock(&c.source).raw();
        let s1 = c.process(&mut inner, raw(&c), 100.0);
        assert!(s1.stations[0].stats.mbps_tx.is_none());
        let s2 = c.process(&mut inner, raw(&c), 102.0);
        let mlo = &s2.stations[0];
        assert!(mlo.mlo && mlo.links.len() == 3);
        assert!(mlo.stats.mbps_tx.unwrap() > 100.0);
        assert!(mlo.links[2].stats.mbps_tx.unwrap() > mlo.links[0].stats.mbps_tx.unwrap());
        assert_eq!(mlo.links[1].band, Some("5G"));
        assert_eq!(s2.radios.len(), 3);
        assert!(s2.radios.iter().all(|r| r.busy_pct.is_some() && r.stations >= 1));
        assert!(inner.history.contains_key("sta:a4:83:e7:10:20:30:tx"));
        // state is valid JSON
        serde_json::from_str::<serde_json::Value>(&serde_json::to_string(&s2).unwrap()).unwrap();
    }
}
