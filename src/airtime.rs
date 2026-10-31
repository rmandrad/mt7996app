//! Per-client airtime fairness: find clients that hog airtime while moving
//! little data (slow rates) and lower their airtime weight; restore it when
//! they calm down. `decide` is pure; applying goes through `control`.
use crate::control::Action;
use crate::model::Station;
use serde::Serialize;
use std::collections::{HashMap, HashSet};

#[derive(Clone, Debug)]
pub struct AirtimeCfg {
    pub default_weight: u32,
    pub floor_weight: u32,
    /// airtime share (of the used airtime) at which a client may count as a hog
    pub hog_share: f64,
    /// a hog must also be this much less efficient (Mbit/s per % airtime) than the median of the others
    pub eff_ratio: f64,
    /// consecutive samples before throttling / before restoring
    pub hold: u32,
    pub release: u32,
    /// below this total utilisation of the interval nothing is done
    pub min_util: f64,
}

impl Default for AirtimeCfg {
    fn default() -> Self {
        Self { default_weight: 256, floor_weight: 32, hog_share: 0.5, eff_ratio: 0.5, hold: 3, release: 5, min_util: 0.05 }
    }
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct AirtimeAdvice {
    pub mac: String,
    pub iface: String,
    pub share_pct: f64,
    pub mbps: f64,
    /// Mbit/s moved per percent of airtime used
    pub efficiency: Option<f64>,
    pub weight: Option<u32>,
    pub suggested: u32,
    pub hog: bool,
    pub throttled: bool,
    pub reason: String,
}

#[derive(Default)]
pub struct AirtimeCtl {
    pub cfg: AirtimeCfg,
    /// only an auto-mode controller emits actions / tracks throttled clients
    pub auto: bool,
    prev: HashMap<String, (f64, u64)>,
    hog_ticks: HashMap<String, u32>,
    calm_ticks: HashMap<String, u32>,
    /// clients we lowered, mac -> iface
    throttled: HashMap<String, String>,
}

fn median(v: &mut [f64]) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    Some(v[v.len() / 2])
}

impl AirtimeCtl {
    pub fn new(cfg: AirtimeCfg, auto: bool) -> Self {
        Self { cfg, auto, ..Default::default() }
    }

    /// One evaluation per sample. Returns the advice table and the actions
    /// that the auto mode should apply.
    pub fn step(&mut self, now: f64, stations: &[Station]) -> (Vec<AirtimeAdvice>, Vec<Action>) {
        let c = self.cfg.clone();
        let mut rows: Vec<(&Station, f64, f64)> = vec![]; // station, delta airtime us, dt
        for s in stations {
            let total = match (s.stats.tx_airtime_us, s.stats.rx_airtime_us) {
                (Some(t), Some(r)) => t + r,
                (Some(t), None) | (None, Some(t)) => t,
                _ => continue,
            };
            let key = format!("{}|{}", s.iface, s.mac);
            if let Some((pts, pv)) = self.prev.insert(key, (now, total)) {
                if now > pts && total >= pv {
                    rows.push((s, (total - pv) as f64, now - pts));
                }
            }
        }
        let present: HashSet<String> = stations.iter().map(|s| format!("{}|{}", s.iface, s.mac)).collect();
        self.prev.retain(|k, _| present.contains(k));
        self.throttled.retain(|mac, iface| present.contains(&format!("{iface}|{mac}")));

        let used: f64 = rows.iter().map(|r| r.1).sum();
        let dt = rows.iter().map(|r| r.2).fold(0.0, f64::max);
        let util = if dt > 0.0 { used / (dt * 1e6) } else { 0.0 };
        let active = |r: &(&Station, f64, f64)| r.1 / (r.2 * 1e6) >= 0.02;
        let n_active = rows.iter().filter(|r| active(r)).count();
        let mut advice = vec![];
        let mut actions = vec![];

        let eff = |r: &(&Station, f64, f64)| {
            let share = r.1 / used * 100.0;
            let mbps = r.0.stats.mbps_tx.unwrap_or(0.0) + r.0.stats.mbps_rx.unwrap_or(0.0);
            (share > 0.0).then(|| mbps / share)
        };

        for r in &rows {
            let s = r.0;
            let share = if used > 0.0 { r.1 / used } else { 0.0 };
            let mbps = s.stats.mbps_tx.unwrap_or(0.0) + s.stats.mbps_rx.unwrap_or(0.0);
            let my_eff = eff(r);
            let mut others: Vec<f64> = rows.iter().filter(|o| o.0.mac != s.mac && active(o)).filter_map(eff).collect();
            let med = median(&mut others);
            let fair = if n_active > 0 { 1.0 / n_active as f64 } else { 1.0 };
            let is_hog = util >= c.min_util
                && n_active >= 2
                && active(r)
                && share >= c.hog_share
                && matches!((my_eff, med), (Some(e), Some(m)) if e < c.eff_ratio * m);
            let suggested = if is_hog {
                ((c.default_weight as f64 * fair / share).round() as u32).clamp(c.floor_weight, c.default_weight)
            } else {
                s.stats.airtime_weight.unwrap_or(c.default_weight)
            };
            let throttled = self.throttled.contains_key(&s.mac);
            let reason = if is_hog {
                format!("uses {:.0}% of the airtime for {:.1} Mbit/s ({:.2} Mbit/s per % vs {:.2} for the others)", share * 100.0, mbps, my_eff.unwrap_or(0.0), med.unwrap_or(0.0))
            } else if throttled {
                "throttled by this daemon".to_string()
            } else {
                String::new()
            };
            advice.push(AirtimeAdvice {
                mac: s.mac.clone(),
                iface: s.iface.clone(),
                share_pct: (share * 1000.0).round() / 10.0,
                mbps: (mbps * 100.0).round() / 100.0,
                efficiency: my_eff.map(|e| (e * 100.0).round() / 100.0),
                weight: s.stats.airtime_weight,
                suggested,
                hog: is_hog,
                throttled,
                reason,
            });

            // hysteresis -> actions
            if is_hog {
                self.calm_ticks.remove(&s.mac);
                let t = self.hog_ticks.entry(s.mac.clone()).or_default();
                *t += 1;
                if self.auto && *t >= c.hold && !throttled && s.stats.airtime_weight != Some(suggested) {
                    self.throttled.insert(s.mac.clone(), s.iface.clone());
                    actions.push(Action::SetAirtime { iface: s.iface.clone(), mac: s.mac.clone(), weight: suggested });
                }
            } else {
                self.hog_ticks.remove(&s.mac);
                if throttled {
                    let t = self.calm_ticks.entry(s.mac.clone()).or_default();
                    *t += 1;
                    if *t >= c.release {
                        self.calm_ticks.remove(&s.mac);
                        self.throttled.remove(&s.mac);
                        actions.push(Action::SetAirtime { iface: s.iface.clone(), mac: s.mac.clone(), weight: c.default_weight });
                    }
                }
            }
        }
        advice.sort_by(|a, b| b.share_pct.total_cmp(&a.share_pct));
        (advice, actions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Stats;

    fn sta(mac: &str, tx_us: u64, rx_us: u64, mbps: f64, w: u32) -> Station {
        Station {
            mac: mac.into(),
            iface: "wlan0".into(),
            stats: Stats { tx_airtime_us: Some(tx_us), rx_airtime_us: Some(rx_us), mbps_tx: Some(mbps), airtime_weight: Some(w), ..Default::default() },
            ..Default::default()
        }
    }

    /// iot hogs 1.2 s of every 2 s tick at 1.5 Mbit/s, laptop uses 0.4 s at 300 Mbit/s
    fn tick(i: u64, iot_w: u32) -> Vec<Station> {
        vec![sta("aa:aa:aa:aa:aa:01", i * 800_000, i * 400_000, 1.5, iot_w), sta("bb:bb:bb:bb:bb:02", i * 300_000, i * 100_000, 300.0, 256)]
    }

    #[test]
    fn throttles_after_hold_and_restores_after_release() {
        let mut c = AirtimeCtl::new(AirtimeCfg::default(), true);
        let mut acts = vec![];
        for i in 0..6u64 {
            let (adv, a) = c.step(i as f64 * 2.0, &tick(i, 256));
            acts.push(a);
            if i == 1 {
                let hog = adv.iter().find(|x| x.mac == "aa:aa:aa:aa:aa:01").unwrap();
                assert!(hog.hog, "{hog:?}");
                assert_eq!(hog.share_pct, 75.0);
                assert!(hog.suggested < 256 && hog.suggested >= 32);
            }
        }
        // hold = 3 hog samples: first delta is at i=1 -> third hog sample at i=3
        assert!(acts[..3].iter().all(Vec::is_empty));
        match &acts[3][..] {
            [Action::SetAirtime { mac, weight, .. }] => {
                assert_eq!(mac, "aa:aa:aa:aa:aa:01");
                assert_eq!(*weight, 171); // 256 * (1/2) / 0.75, rounded
            }
            other => panic!("{other:?}"),
        }
        assert!(acts[4].is_empty() && acts[5].is_empty()); // no repeated actions

        // the hog stops: both light users -> restore after `release` calm samples
        let mut restored = None;
        let base = 5u64;
        for j in 1..=8u64 {
            let t = (base + j) as f64 * 2.0;
            let iot = sta("aa:aa:aa:aa:aa:01", base * 1_200_000 + j * 10_000, 0, 0.1, 85);
            let lap = sta("bb:bb:bb:bb:bb:02", base * 400_000 + j * 600_000, 0, 300.0, 256);
            let (_, a) = c.step(t, &[iot, lap]);
            if !a.is_empty() {
                restored = Some((j, a));
            }
        }
        let (j, a) = restored.expect("restored");
        assert_eq!(j, 5);
        assert_eq!(a, vec![Action::SetAirtime { iface: "wlan0".into(), mac: "aa:aa:aa:aa:aa:01".into(), weight: 256 }]);
    }

    #[test]
    fn single_client_or_idle_is_never_a_hog() {
        let mut c = AirtimeCtl::new(AirtimeCfg::default(), true);
        for i in 0..5u64 {
            let (adv, a) = c.step(i as f64 * 2.0, &[sta("aa:aa:aa:aa:aa:01", i * 1_500_000, 0, 1.0, 256)]);
            assert!(a.is_empty() && adv.iter().all(|x| !x.hog));
        }
        let mut c = AirtimeCtl::new(AirtimeCfg::default(), true);
        for i in 0..5u64 {
            let (_, a) = c.step(i as f64 * 2.0, &[sta("aa:aa:aa:aa:aa:01", i * 20_000, 0, 0.0, 256), sta("bb:bb:bb:bb:bb:02", i * 1_000, 0, 0.0, 256)]);
            assert!(a.is_empty());
        }
    }

    #[test]
    fn counter_reset_is_ignored() {
        let mut c = AirtimeCtl::new(AirtimeCfg::default(), true);
        c.step(0.0, &tick(10, 256));
        let (adv, _) = c.step(2.0, &tick(1, 256)); // counters went backwards
        assert!(adv.is_empty());
    }
}
