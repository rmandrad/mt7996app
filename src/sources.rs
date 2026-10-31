//! Raw data sources: `RealSource` (iw + debugfs) and `DemoSource` (synthetic).
use crate::debugfs;
use crate::iwparse;
use crate::model::*;
use std::collections::HashMap;
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub struct RealSource {
    pub iw: String,
    pub debugfs_root: String,
    pub any_driver: bool,
    pub timeout: Duration,
}

impl RealSource {
    pub fn new(iw: String, debugfs_root: String, any_driver: bool) -> Self {
        Self { iw, debugfs_root, any_driver, timeout: Duration::from_secs(5) }
    }

    fn run(&self, args: &[&str]) -> Result<String, String> {
        let label = format!("{} {}", self.iw, args.join(" "));
        let mut child = Command::new(&self.iw)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("{label}: {e}"))?;
        // iw output is small; read it from threads-free polling so we can time out.
        let mut out = child.stdout.take().unwrap();
        let mut err = child.stderr.take().unwrap();
        let rd_out = std::thread::spawn(move || {
            let mut s = String::new();
            let _ = out.read_to_string(&mut s);
            s
        });
        let rd_err = std::thread::spawn(move || {
            let mut s = String::new();
            let _ = err.read_to_string(&mut s);
            s
        });
        let start = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(st)) => break st,
                Ok(None) if start.elapsed() > self.timeout => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("{label}: timed out"));
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(e) => return Err(format!("{label}: {e}")),
            }
        };
        let (so, se) = (rd_out.join().unwrap_or_default(), rd_err.join().unwrap_or_default());
        if status.success() {
            Ok(so)
        } else {
            Err(format!("{label}: {}", if se.trim().is_empty() { status.to_string() } else { se.trim().to_string() }))
        }
    }
}

impl Source for RealSource {
    fn demo(&self) -> bool {
        false
    }

    fn raw(&mut self) -> Raw {
        let mut raw = Raw::default();
        let mut phys = debugfs::find_phys();
        match self.run(&["dev"]) {
            Ok(t) => raw.ifaces = iwparse::parse_iw_dev(&t),
            Err(e) => raw.errors.push(e),
        }
        if self.any_driver {
            phys = raw.ifaces.iter().map(|i| format!("phy{}", i.phy)).collect();
            phys.sort();
            phys.dedup();
        } else {
            if phys.is_empty() {
                raw.errors.push(
                    "no mt7996 phy found in /sys/class/ieee80211 (use --any-driver to show every wifi phy)".into(),
                );
            }
            raw.ifaces.retain(|i| phys.contains(&format!("phy{}", i.phy)));
        }

        let ifaces = raw.ifaces.clone();
        for i in &ifaces {
            if !matches!(i.kind.as_deref(), None | Some("AP") | Some("mesh point") | Some("P2P-GO")) {
                continue;
            }
            match self.run(&["dev", &i.name, "station", "dump"]) {
                Err(e) => raw.errors.push(e),
                Ok(t) => {
                    for mut s in iwparse::parse_station_dump(&t) {
                        s.phy = format!("phy{}", i.phy);
                        for (id, addr) in debugfs::read_sta_links(&s.phy, &s.iface, &s.mac, &self.debugfs_root) {
                            if !s.links.iter().any(|l| l.id == id) {
                                s.links.push(Link { id, ..Default::default() });
                            }
                            let l = s.links.iter_mut().find(|l| l.id == id).unwrap();
                            if l.stats.addr.is_none() {
                                l.stats.addr = addr;
                            }
                        }
                        s.links.sort_by_key(|l| l.id);
                        s.mlo = s.links.len() > 1;
                        raw.stations.push(s);
                    }
                }
            }
            if let Ok(t) = self.run(&["dev", &i.name, "survey", "dump"]) {
                raw.survey.insert(i.name.clone(), iwparse::parse_survey(&t));
            }
        }
        raw.hw = phys.iter().map(|p| debugfs::read_hw(p, &self.debugfs_root)).collect();
        if !raw.hw.is_empty() && !raw.hw.iter().any(|h| h.readable) {
            raw.errors.push(format!(
                "debugfs not readable under {} (run as root, mount debugfs); hardware panels disabled",
                self.debugfs_root
            ));
        }
        raw
    }
}

// ---------------------------------------------------------------- demo ----

/// Synthetic AP: one 3-link MLD plus two single-link clients.
pub struct DemoSource {
    rng: u64,
    t0: u64,
    ctr: HashMap<String, u64>,
    survey: HashMap<u32, [u64; 4]>,
}

const DEMO_LINKS: [(u32, u32, u32, u32, &str, f64); 3] = [
    (2437, 6, 40, 2447, "2.4G", 20.0),
    (5180, 36, 80, 5210, "5G", 23.0),
    (5975, 5, 160, 6025, "6G", 17.0),
];

impl DemoSource {
    pub fn new() -> Self {
        let t0 = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        Self { rng: 0x9E3779B97F4A7C15, t0, ctr: HashMap::new(), survey: HashMap::new() }
    }

    fn rand(&mut self) -> f64 {
        // xorshift64*
        self.rng ^= self.rng >> 12;
        self.rng ^= self.rng << 25;
        self.rng ^= self.rng >> 27;
        (self.rng.wrapping_mul(0x2545F4914F6CDD1D) >> 11) as f64 / (1u64 << 53) as f64
    }

    fn gauss(&mut self, mean: f64, sd: f64) -> f64 {
        let s: f64 = (0..12).map(|_| self.rand()).sum::<f64>() - 6.0;
        mean + sd * s
    }

    fn int(&mut self, lo: i32, hi: i32) -> i32 {
        lo + (self.rand() * (hi - lo + 1) as f64) as i32
    }

    fn bump(&mut self, key: String, step: f64) -> u64 {
        let v = self.ctr.entry(key).or_insert(0);
        *v += step.max(0.0) as u64;
        *v
    }

    fn rate(mbps: f64, mcs: u32, width: u32, gi: &str, proto: &str) -> Rate {
        Rate { raw: "demo".into(), mbps: Some(mbps), mcs: Some(mcs), nss: Some(2), width: Some(width), gi: Some(gi.into()), proto: proto.into() }
    }

    fn link_stats(&mut self, mac: &str, lid: u32, sig: i32, mbps: f64, retry_pct: f64) -> Stats {
        let tx_mbps = self.gauss(mbps, mbps * 0.15).max(0.0);
        let b_tx = tx_mbps * 1e6 / 8.0 * 2.0; // bytes per 2 s tick
        let b_rx = b_tx * 0.2;
        let pk_tx = b_tx / 1200.0;
        let k = |s: &str| format!("{mac}/{lid}/{s}");
        let sig = sig + self.int(-2, 2);
        Stats {
            signal: Some(sig),
            tx_bytes: Some(self.bump(k("tb"), b_tx)),
            rx_bytes: Some(self.bump(k("rb"), b_rx)),
            tx_pkts: Some(self.bump(k("tp"), pk_tx)),
            rx_pkts: Some(self.bump(k("rp"), b_rx / 1200.0)),
            tx_retries: Some(self.bump(k("tr"), pk_tx * retry_pct / 100.0)),
            tx_failed: Some(self.bump(k("tf"), pk_tx * retry_pct / 4000.0)),
            ..Default::default()
        }
    }
}

impl Source for DemoSource {
    fn demo(&self) -> bool {
        true
    }

    fn raw(&mut self) -> Raw {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let mut iface = Iface {
            name: "wlan0".into(),
            phy: 0,
            kind: Some("AP".into()),
            ssid: Some("mt7996-demo".into()),
            ..Default::default()
        };
        iface.base.addr = Some("02:00:00:00:00:00".into());
        for (lid, (f, ch, w, c1, _, tx)) in DEMO_LINKS.iter().enumerate() {
            iface.links.insert(
                lid as u32,
                IfLink {
                    freq: Some(*f),
                    channel: Some(*ch),
                    width: Some(*w),
                    center1: Some(*c1),
                    txpower: Some(*tx),
                    addr: Some(format!("02:00:00:00:00:0{}", lid + 1)),
                    ..Default::default()
                },
            );
        }

        // MLO phone: the 6 GHz link carries most traffic, 2.4 GHz is nearly idle.
        let mac = "a4:83:e7:10:20:30";
        let mut sta = Station { mac: mac.into(), iface: "wlan0".into(), phy: "phy0".into(), mlo: true, ..Default::default() };
        let rates = [(286.7, 9, 40, "be"), (1201.0, 11, 80, "be"), (2882.0, 13, 160, "be")];
        for (lid, (sig, mbps, rt)) in [(-58, 4.0, 12.0), (-52, 180.0, 6.0), (-49, 620.0, 9.0)].into_iter().enumerate() {
            let mut st = self.link_stats(mac, lid as u32, sig, mbps, rt);
            let (rm, mcs, w, p) = rates[lid];
            st.tx_rate = Some(Self::rate(rm, mcs, w, "0", p));
            st.rx_rate = Some(Self::rate(rm * 0.8, mcs, w, "0", p));
            st.addr = Some(format!("a6:83:e7:10:20:3{lid}"));
            sta.links.push(Link { id: lid as u32, stats: st, ..Default::default() });
        }
        let sum = |f: fn(&Stats) -> Option<u64>| Some(sta.links.iter().filter_map(|l| f(&l.stats)).sum::<u64>());
        let best = sta.links[2].stats.clone();
        sta.stats = Stats {
            tx_bytes: sum(|s| s.tx_bytes),
            rx_bytes: sum(|s| s.rx_bytes),
            tx_pkts: sum(|s| s.tx_pkts),
            rx_pkts: sum(|s| s.rx_pkts),
            tx_retries: sum(|s| s.tx_retries),
            tx_failed: sum(|s| s.tx_failed),
            signal: best.signal,
            signal_avg: best.signal,
            chains: vec![best.signal.unwrap() - 1, best.signal.unwrap() + 2],
            tx_rate: best.tx_rate,
            rx_rate: best.rx_rate,
            inactive_ms: Some(20),
            connected_s: Some(now.saturating_sub(self.t0) + 3600),
            authorized: Some(true),
            ..Default::default()
        };

        let single = |this: &mut Self, mac: &str, lid: u32, sig: i32, mbps: f64, rt: f64, rate: Rate| {
            let mut st = this.link_stats(mac, lid, sig, mbps, rt);
            st.chains = vec![st.signal.unwrap(), st.signal.unwrap() - 3];
            st.signal_avg = st.signal;
            st.tx_rate = Some(rate.clone());
            st.rx_rate = Some(rate);
            st.inactive_ms = Some(this.int(0, 800) as u64);
            st.connected_s = Some(5400);
            st.authorized = Some(true);
            Station { mac: mac.into(), iface: "wlan0".into(), phy: "phy0".into(), stats: st, ..Default::default() }
        };
        let laptop = single(self, "3c:22:fb:aa:bb:cc", 1, -61, 320.0, 18.0, Self::rate(864.7, 9, 80, "0", "ax"));
        let mut iot_rate = Self::rate(72.2, 7, 20, "short", "n");
        iot_rate.nss = Some(1);
        let iot = single(self, "b8:27:eb:01:02:03", 0, -72, 1.5, 30.0, iot_rate);

        let mut survey = vec![];
        for (f, base, noise) in [(2437u32, 38, -92), (5180, 17, -95), (5975, 5, -97)] {
            let busy = (2000.0 * (base + self.int(-4, 4)).clamp(0, 95) as f64 / 100.0) as u64;
            let c = self.survey.entry(f).or_insert([0; 4]);
            c[0] += 2000;
            c[1] += busy;
            c[2] += busy * 3 / 10;
            c[3] += busy / 4;
            survey.push(SurveyChan {
                freq: f,
                in_use: true,
                noise: Some(noise),
                active_ms: Some(c[0]),
                busy_ms: Some(c[1]),
                rx_ms: Some(c[2]),
                tx_ms: Some(c[3]),
            });
        }

        let per = [11u32, 5, 2];
        let mut tx = vec![];
        for b in 0..3u32 {
            let step = 80000.0 * (1 + b) as f64;
            let att = self.bump(format!("att{b}"), step);
            let ok = self.bump(format!("ok{b}"), step * (100 - per[b as usize]) as f64 / 100.0);
            tx.push(TxStats {
                phy: format!("phy{b}"),
                band: b,
                attempts: Some(att),
                success: Some(ok),
                per: Some(per[b as usize]),
                ba_miss: Some(att / 400),
                amsdu: vec![60 - 15 * b as u64, 20, 10, 6 + 4 * b as u64, 2, 1, 1, 0],
                ampdu_len: ["1", "2-4", "5-8", "9-16", "17-32", "33-64"].map(String::from).to_vec(),
                ampdu_cnt: vec![200, 300, 800, 2500, 9000, 24000],
            });
        }
        let queues = ["MAIN0", "MAIN1", "MAIN2", "MCUWM", "MCUWA"]
            .iter()
            .map(|q| Queue { queue: q.to_string(), queued: self.int(0, 40) as i64, head: 100, tail: 90 })
            .collect();
        let hw = Hw {
            phy: "phy0".into(),
            readable: true,
            tx,
            queues,
            twt: vec![Twt { wcid: 3, id: 1, flags: "spt-a".into(), exp: 10, mantissa: 512, duration: 64, tsf: 123456789 }],
            rdd: Some("not running".into()),
            ..Default::default()
        };

        Raw {
            ifaces: vec![iface],
            stations: vec![sta, laptop, iot],
            survey: HashMap::from([("wlan0".to_string(), survey)]),
            hw: vec![hw],
            errors: vec![],
        }
    }
}
