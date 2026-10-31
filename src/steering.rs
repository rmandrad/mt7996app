//! Band-steering assistant: suggests (and optionally sends) 802.11v BSS
//! transition requests for *single-link* clients on a congested link when a
//! better link of the same AP is available. MLO clients are never touched.
use crate::control::Action;
use crate::model::{Radio, Station};
use serde::Serialize;
use std::collections::HashMap;

#[derive(Clone, Debug)]
pub struct SteerCfg {
    /// source link must be at least this busy (%) ...
    pub busy_hi: f64,
    /// ... and the target at most this busy
    pub busy_lo: f64,
    /// minimum SNR (dB) on the current link, as a proxy for "close enough to move up a band"
    pub min_snr: i32,
    pub cooldown_s: f64,
}

impl Default for SteerCfg {
    fn default() -> Self {
        Self { busy_hi: 60.0, busy_lo: 30.0, min_snr: 25, cooldown_s: 600.0 }
    }
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct SteerAdvice {
    pub mac: String,
    pub iface: String,
    pub from_link: u32,
    pub from_band: Option<&'static str>,
    pub to_link: u32,
    pub to_band: Option<&'static str>,
    pub reason: String,
    /// "low" | "medium": whether the client is likely to support the target band
    pub confidence: &'static str,
    pub neighbor: String,
    pub cooldown_left_s: f64,
}

#[derive(Default)]
pub struct SteerCtl {
    pub cfg: SteerCfg,
    last: HashMap<String, f64>,
}

/// 802.11 Annex E operating class for a channel (subset, 20..320 MHz).
pub fn opclass(freq: u32, width: u32) -> u32 {
    match (freq, width) {
        (f, _) if f < 3000 => if width >= 40 { 83 } else { 81 },
        (f, 20) if f < 5925 => match f { 5180..=5240 => 115, 5260..=5320 => 118, 5500..=5720 => 121, _ => 125 },
        (f, 40) if f < 5925 => match f { 5180..=5240 => 116, 5260..=5320 => 119, 5500..=5720 => 122, _ => 126 },
        (_, 80) if freq < 5925 => 128,
        (_, 160) if freq < 5925 => 129,
        (_, 20) => 131,
        (_, 40) => 132,
        (_, 80) => 133,
        (_, 160) => 134,
        _ => 137,
    }
}

fn phy_type(proto: &str, freq: u32) -> u32 {
    // 802.11 dot11PHYType: ht=7, vht=9, he=14, eht=15 (HE/EHT values unverified)
    match proto {
        "be" => 15,
        "ax" => 14,
        "ac" => 9,
        "n" => 7,
        _ if freq < 3000 => 6,
        _ => 4,
    }
}

impl SteerCtl {
    pub fn new(cfg: SteerCfg) -> Self {
        Self { cfg, ..Default::default() }
    }

    pub fn mark_sent(&mut self, mac: &str, now: f64) {
        self.last.insert(mac.to_string(), now);
    }

    pub fn advise(&self, now: f64, stations: &[Station], radios: &[Radio]) -> Vec<(SteerAdvice, Action)> {
        let c = &self.cfg;
        let mut out = vec![];
        for s in stations {
            if s.mlo {
                continue;
            }
            let rs: Vec<&Radio> = radios.iter().filter(|r| r.iface == s.iface).collect();
            let from_id = match (s.links.len(), rs.len()) {
                (1, _) => Some(s.links[0].id),
                (0, 1) => rs[0].link.or(Some(0)),
                _ => None,
            };
            let Some(from_id) = from_id else { continue };
            let Some(from) = rs.iter().find(|r| r.link == Some(from_id) || (r.link.is_none() && rs.len() == 1)) else { continue };
            let signal = s.links.first().and_then(|l| l.stats.signal).or(s.stats.signal);
            let (Some(sig), Some(noise)) = (signal, from.noise) else { continue };
            let snr = sig - noise;
            let proto = s.stats.tx_rate.as_ref().map(|r| r.proto.as_str()).unwrap_or("legacy");
            if from.busy_pct.unwrap_or(0.0) < c.busy_hi || snr < c.min_snr {
                continue;
            }
            let rank = |b: Option<&str>| match b { Some("2.4G") => 0, Some("5G") => 1, Some("6G") => 2, _ => 0 };
            let target = rs
                .iter()
                .filter(|r| r.link.is_some() && r.link != Some(from_id) && rank(r.band) > rank(from.band))
                .filter(|r| r.busy_pct.is_some_and(|b| b <= c.busy_lo) && r.addr.is_some() && r.freq.is_some())
                .filter(|r| match r.band { Some("6G") => proto == "be", _ => matches!(proto, "ac" | "ax" | "be") })
                .min_by(|a, b| a.busy_pct.partial_cmp(&b.busy_pct).unwrap_or(std::cmp::Ordering::Equal));
            let Some(t) = target else { continue };
            let freq = t.freq.unwrap();
            let neighbor = format!(
                "{},0x0000,{},{},{}",
                t.addr.as_deref().unwrap_or("").to_lowercase(),
                opclass(freq, t.width.unwrap_or(20)),
                t.channel.unwrap_or(0),
                phy_type(proto, freq)
            );
            let left = self.last.get(&s.mac).map_or(0.0, |l| (c.cooldown_s - (now - l)).max(0.0));
            let advice = SteerAdvice {
                mac: s.mac.clone(),
                iface: s.iface.clone(),
                from_link: from_id,
                from_band: from.band,
                to_link: t.link.unwrap(),
                to_band: t.band,
                reason: format!(
                    "{} link is {:.0}% busy, {} link only {:.0}%; client SNR {} dB, 802.11{}",
                    from.band.unwrap_or("?"), from.busy_pct.unwrap_or(0.0), t.band.unwrap_or("?"), t.busy_pct.unwrap_or(0.0), snr, proto
                ),
                confidence: if t.band == Some("6G") { "low" } else { "medium" },
                neighbor: neighbor.clone(),
                cooldown_left_s: left.round(),
            };
            out.push((advice, Action::Btm { iface: s.iface.clone(), mac: s.mac.clone(), neighbor }));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Link, Rate, Stats};

    fn radio(link: u32, band: &'static str, freq: u32, ch: u32, w: u32, busy: f64, noise: i32) -> Radio {
        Radio {
            iface: "wlan0".into(),
            link: Some(link),
            band: Some(band),
            freq: Some(freq),
            channel: Some(ch),
            width: Some(w),
            busy_pct: Some(busy),
            noise: Some(noise),
            addr: Some(format!("02:00:00:00:00:0{}", link + 1)),
            ..Default::default()
        }
    }

    fn sta(mlo: bool, links: &[u32], sig: i32, proto: &str) -> Station {
        Station {
            mac: "10:91:d1:f7:5e:bd".into(),
            iface: "wlan0".into(),
            mlo,
            links: links.iter().map(|id| Link { id: *id, stats: Stats { signal: Some(sig), ..Default::default() }, ..Default::default() }).collect(),
            stats: Stats { signal: Some(sig), tx_rate: Some(Rate { proto: proto.into(), ..Default::default() }), ..Default::default() },
            ..Default::default()
        }
    }

    #[test]
    fn steers_single_link_client_to_quiet_higher_band() {
        let radios = [radio(0, "2.4G", 2437, 6, 20, 75.0, -90), radio(1, "5G", 5180, 36, 80, 12.0, -94)];
        let c = SteerCtl::new(SteerCfg::default());
        let v = c.advise(1000.0, &[sta(false, &[0], -55, "ax")], &radios);
        assert_eq!(v.len(), 1);
        let (a, act) = &v[0];
        assert_eq!((a.from_link, a.to_link, a.confidence), (0, 1, "medium"));
        assert_eq!(a.neighbor, "02:00:00:00:00:02,0x0000,128,36,14");
        assert!(matches!(act, Action::Btm { neighbor, .. } if neighbor == &a.neighbor));
    }

    #[test]
    fn never_touches_mlo_or_low_snr_or_idle_links() {
        let radios = [radio(0, "2.4G", 2437, 6, 20, 75.0, -90), radio(1, "5G", 5180, 36, 80, 12.0, -94)];
        let c = SteerCtl::new(SteerCfg::default());
        assert!(c.advise(0.0, &[sta(true, &[0, 1], -55, "be")], &radios).is_empty());
        assert!(c.advise(0.0, &[sta(false, &[0], -75, "ax")], &radios).is_empty()); // SNR 15
        let calm = [radio(0, "2.4G", 2437, 6, 20, 20.0, -90), radio(1, "5G", 5180, 36, 80, 12.0, -94)];
        assert!(c.advise(0.0, &[sta(false, &[0], -55, "ax")], &calm).is_empty());
        // 802.11n client is not assumed to support 5 GHz
        assert!(c.advise(0.0, &[sta(false, &[0], -55, "n")], &radios).is_empty());
        // link unknown on a multi-link AP: do nothing
        assert!(c.advise(0.0, &[sta(false, &[], -55, "ax")], &radios).is_empty());
    }

    #[test]
    fn six_ghz_needs_wifi7_and_cooldown_is_reported() {
        let radios = [radio(1, "5G", 5180, 36, 80, 80.0, -94), radio(2, "6G", 5975, 5, 160, 3.0, -97)];
        let mut c = SteerCtl::new(SteerCfg::default());
        assert!(c.advise(0.0, &[sta(false, &[1], -55, "ax")], &radios).is_empty());
        let v = c.advise(0.0, &[sta(false, &[1], -55, "be")], &radios);
        assert_eq!((v[0].0.to_band, v[0].0.confidence), (Some("6G"), "low"));
        c.mark_sent("10:91:d1:f7:5e:bd", 100.0);
        let v = c.advise(160.0, &[sta(false, &[1], -55, "be")], &radios);
        assert_eq!(v[0].0.cooldown_left_s, 540.0);
    }

    #[test]
    fn operating_classes() {
        assert_eq!(opclass(2437, 20), 81);
        assert_eq!(opclass(5180, 80), 128);
        assert_eq!(opclass(5500, 160), 129);
        assert_eq!(opclass(5975, 160), 134);
        assert_eq!(opclass(6105, 320), 137);
    }
}
