//! Channel / interference planner with preamble-puncturing recommendations.
//!
//! Inputs are the kernel's scan cache (`iw dev X scan dump`), the survey and
//! the channel list of the phy. Everything here is advisory and pure.
//!
//! Model (deliberately simple, documented in the README):
//! * A 20 MHz "bin" is *busy* if a neighbouring BSS is heard on it at
//!   >= -82 dBm (the 802.11 preamble-detect CCA level).
//! * Without puncturing, a transmission can only use the largest aligned block
//!   around the primary 20 MHz that contains no busy bin.
//! * With puncturing, all non-busy bins stay usable, if the pattern is one the
//!   kernel accepts (tables ported from net/wireless/chan.c).
use crate::model::*;
use serde::Serialize;
use std::collections::{HashMap, HashSet};

const BUSY_DBM: f64 = -82.0;
const MIN_PUNCT_GAIN_MHZ: i32 = 20;
const SWITCH_MARGIN: f64 = 0.75;
const DFS_PENALTY: f64 = 0.75;
const MAX_SCAN_AGE_MS: u64 = 15 * 60 * 1000;

// net/wireless/chan.c: puncturing_values_{80,160,320}mhz (bit i = i-th 20 MHz
// bin counted from the lowest frequency of the block).
const PUNCT_80: &[u32] = &[0x8, 0x4, 0x2, 0x1];
const PUNCT_160: &[u32] = &[0x80, 0x40, 0x20, 0x10, 0x8, 0x4, 0x2, 0x1, 0xc0, 0x30, 0xc, 0x3];
const PUNCT_320: &[u32] = &[
    0xc000, 0x3000, 0xc00, 0x300, 0xc0, 0x30, 0xc, 0x3, 0xf000, 0xf00, 0xf0, 0xf, 0xfc00, 0xf300, 0xf0c0, 0xf030,
    0xf00c, 0xf003, 0xc00f, 0x300f, 0xc0f, 0x30f, 0xcf, 0x3f,
];

fn punct_table(width: u32) -> &'static [u32] {
    match width {
        80 => PUNCT_80,
        160 => PUNCT_160,
        320 => PUNCT_320,
        _ => &[],
    }
}

/// Same check as the kernel's `valid_puncturing_bitmap()`.
pub fn valid_puncturing(width: u32, center1: u32, primary: u32, punct: u32) -> bool {
    if punct == 0 {
        return true;
    }
    let table = punct_table(width);
    if table.is_empty() {
        return false;
    }
    let start = center1 as i64 - (width / 2) as i64;
    let bit = (primary as i64 - start).div_euclid(20);
    if !(0..16).contains(&bit) || punct & (1 << bit) != 0 {
        return false;
    }
    table.contains(&punct)
}

pub fn bins_of(freq: u32, width: u32, center1: Option<u32>) -> Vec<u32> {
    match center1 {
        Some(c) if width >= 40 => (0..width / 20).map(|i| c - width / 2 + 10 + 20 * i).collect(),
        _ => vec![freq],
    }
}

fn weight(sig: f64) -> f64 {
    ((sig + 95.0) / 35.0).clamp(0.0, 1.0).powi(2)
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct BinInfo {
    pub freq: u32,
    pub neighbors: usize,
    pub strongest: Option<f64>,
    pub load: f64,
    pub busy: bool,
    pub primary: bool,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct PunctRec {
    pub bitmap: u32,
    pub bitmap_hex: String,
    pub clear_before_mhz: u32,
    pub clear_after_mhz: u32,
    pub gain_mhz: i32,
    pub punctured_freqs: Vec<u32>,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Candidate {
    pub freq: u32,
    pub channel: Option<u32>,
    pub center1: u32,
    pub width: u32,
    pub score: f64,
    pub dfs: bool,
    pub neighbors: usize,
    pub current: bool,
    pub reason: String,
}

#[derive(Serialize, Clone, Debug)]
pub struct LinkPlan {
    pub iface: String,
    pub link: Option<u32>,
    pub band: Option<&'static str>,
    pub freq: u32,
    pub channel: Option<u32>,
    pub width: u32,
    pub center1: Option<u32>,
    pub bins: Vec<BinInfo>,
    pub clear_mhz: u32,
    pub current_score: f64,
    pub puncture: Option<PunctRec>,
    pub puncture_note: Option<String>,
    pub candidates: Vec<Candidate>,
    pub recommend: Option<Candidate>,
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct Plan {
    pub ts: f64,
    pub scan_age_s: Option<f64>,
    pub notes: Vec<String>,
    pub links: Vec<LinkPlan>,
}

pub struct PlanInput<'a> {
    pub now: f64,
    pub radios: &'a [Radio],
    pub scan: Option<&'a ScanData>,
    pub survey: &'a HashMap<String, Vec<SurveyChan>>,
    pub phy_chans: &'a HashMap<String, Vec<ChanInfo>>,
}

struct Block {
    bins: Vec<u32>,
    center: u32,
    dfs: bool,
}

/// All aligned `width` MHz blocks of fully usable 20 MHz channels.
fn blocks(chans: &[ChanInfo], band: &str, width: u32) -> Vec<Block> {
    let usable: HashMap<u32, &ChanInfo> = chans.iter().filter(|c| !c.disabled).map(|c| (c.freq, c)).collect();
    let in_band = |f: u32| match band {
        "2.4G" => f < 3000,
        "5G" => (3000..5925).contains(&f),
        _ => f >= 5925,
    };
    if band == "2.4G" {
        // only the three non-overlapping 20 MHz channels
        return if width == 20 {
            [2412u32, 2437, 2462]
                .iter()
                .filter(|f| usable.contains_key(f))
                .map(|f| Block { bins: vec![*f], center: *f, dfs: false })
                .collect()
        } else {
            vec![]
        };
    }
    let n = (width / 20).max(1);
    let base_of = |f: u32| -> u32 {
        if f >= 5925 {
            5955
        } else {
            [5180u32, 5500, 5745].iter().rev().find(|b| **b <= f).copied().unwrap_or(5180)
        }
    };
    let mut seen = HashSet::new();
    let mut out = vec![];
    let mut freqs: Vec<u32> = usable.keys().copied().filter(|f| in_band(*f)).collect();
    freqs.sort();
    for f in freqs {
        let base = base_of(f);
        let idx = (f - base) / 20;
        let first = base + (idx / n) * n * 20;
        if !seen.insert(first) {
            continue;
        }
        let bins: Vec<u32> = (0..n).map(|i| first + i * 20).collect();
        if bins.iter().all(|b| usable.contains_key(b)) {
            let dfs = bins.iter().any(|b| usable[b].radar);
            out.push(Block { center: (bins[0] + bins[bins.len() - 1]) / 2, bins, dfs });
        }
    }
    out
}

fn clear_mhz(bins: &[BinInfo]) -> u32 {
    let n = bins.len();
    let Some(p) = bins.iter().position(|b| b.primary) else { return 20 };
    let mut best = 20;
    let mut size = 2;
    while size <= n {
        let start = p / size * size;
        if start + size <= n && bins[start..start + size].iter().all(|b| !b.busy) {
            best = size as u32 * 20;
            size *= 2;
        } else {
            break;
        }
    }
    best
}

fn recommend_puncture(width: u32, center1: u32, primary: u32, bins: &[BinInfo]) -> (Option<PunctRec>, Option<String>) {
    if width < 80 {
        return (None, None);
    }
    let busy: u32 = bins.iter().enumerate().filter(|(_, b)| b.busy).fold(0, |m, (i, _)| m | 1 << i);
    if busy == 0 {
        return (None, None);
    }
    let before = clear_mhz(bins);
    let best = punct_table(width)
        .iter()
        .copied()
        .filter(|p| p & busy == busy && valid_puncturing(width, center1, primary, *p))
        .min_by_key(|p| (p.count_ones(), *p));
    let Some(p) = best else {
        return (None, Some("busy subchannels cannot be covered by a valid puncturing pattern (or the primary 20 MHz is busy)".into()));
    };
    let after = (bins.len() as u32 - p.count_ones()) * 20;
    let gain = after as i32 - before as i32;
    if gain < MIN_PUNCT_GAIN_MHZ {
        return (None, Some(format!("puncturing would only gain {gain} MHz")));
    }
    let freqs = bins.iter().enumerate().filter(|(i, _)| p & (1 << i) != 0).map(|(_, b)| b.freq).collect();
    (
        Some(PunctRec { bitmap: p, bitmap_hex: format!("0x{p:x}"), clear_before_mhz: before, clear_after_mhz: after, gain_mhz: gain, punctured_freqs: freqs }),
        None,
    )
}

fn bin_info(freq: u32, primary: bool, window: f64, bss: &[&Bss]) -> BinInfo {
    let near: Vec<&&Bss> = bss.iter().filter(|b| (b.freq as f64 - freq as f64).abs() < window).collect();
    let strongest = near.iter().map(|b| b.signal).fold(None, |m: Option<f64>, s| Some(m.map_or(s, |x| x.max(s))));
    BinInfo {
        freq,
        neighbors: near.len(),
        strongest,
        load: (near.iter().map(|b| weight(b.signal)).sum::<f64>() * 100.0).round() / 100.0,
        busy: strongest.is_some_and(|s| s >= BUSY_DBM),
        primary,
    }
}

fn survey_busy(survey: &[SurveyChan], freq: u32) -> f64 {
    survey
        .iter()
        .find(|c| c.freq == freq)
        .and_then(|c| Some(c.busy_ms? as f64 / c.active_ms.filter(|a| *a > 0)? as f64))
        .unwrap_or(0.0)
}

pub fn build_plan(inp: &PlanInput) -> Plan {
    let mut plan = Plan { ts: inp.now, ..Default::default() };
    let own: HashSet<String> = inp.radios.iter().filter_map(|r| r.addr.clone()).map(|a| a.to_lowercase()).collect();
    match inp.scan {
        None => plan.notes.push("no scan data yet; the AP radio does not scan by itself - trigger a scan (control) or rely on hostapd ACS".into()),
        Some(s) => {
            plan.scan_age_s = Some((inp.now - s.ts).max(0.0));
            if inp.now - s.ts > 600.0 {
                plan.notes.push("scan data is older than 10 minutes".into());
            }
        }
    }
    for r in inp.radios {
        let (Some(freq), Some(width)) = (r.freq, r.width) else { continue };
        let band = r.band.unwrap_or("5G");
        let window = if band == "2.4G" { 15.0 } else { 10.0 };
        let bss: Vec<&Bss> = inp
            .scan
            .and_then(|s| s.per_iface.get(&r.iface))
            .map(|v| {
                v.iter()
                    .filter(|b| !own.contains(&b.bssid) && b.seen_ms.is_none_or(|m| m <= MAX_SCAN_AGE_MS))
                    .collect()
            })
            .unwrap_or_default();
        let survey = inp.survey.get(&r.iface).map(Vec::as_slice).unwrap_or(&[]);
        let bins: Vec<BinInfo> = bins_of(freq, width, r.center1).into_iter().map(|f| bin_info(f, f == freq, window, &bss)).collect();
        let score_of = |bs: &[BinInfo]| -> f64 {
            ((bs.iter().map(|b| b.load + survey_busy(survey, b.freq)).sum::<f64>()) * 100.0).round() / 100.0
        };
        let current_score = score_of(&bins);
        let (puncture, puncture_note) = match r.center1 {
            Some(c1) => recommend_puncture(width, c1, freq, &bins),
            None => (None, None),
        };

        // candidates: other aligned blocks of the same width in this band
        let chans = r.phy.as_ref().and_then(|p| inp.phy_chans.get(p)).map(Vec::as_slice).unwrap_or(&[]);
        let mut cands: Vec<Candidate> = vec![];
        for b in blocks(chans, band, width) {
            let infos: Vec<BinInfo> = b.bins.iter().map(|f| bin_info(*f, false, window, &bss)).collect();
            let is_cur = b.center == r.center1.unwrap_or(freq) || (width == 20 && b.bins[0] == freq);
            let primary = if is_cur {
                freq
            } else {
                infos.iter().min_by(|a, b| a.load.total_cmp(&b.load).then(a.freq.cmp(&b.freq))).map(|i| i.freq).unwrap_or(b.bins[0])
            };
            let mut score = score_of(&infos);
            if b.dfs && !is_cur {
                score += DFS_PENALTY;
            }
            let nb: usize = infos.iter().map(|i| i.neighbors).sum();
            let strongest = infos.iter().filter_map(|i| i.strongest).fold(None, |m: Option<f64>, s| Some(m.map_or(s, |x| x.max(s))));
            let mut reason = format!("{nb} neighbor BSS");
            if let Some(s) = strongest {
                reason += &format!(", strongest {s:.0} dBm");
            }
            if b.dfs {
                reason += ", DFS (channel availability check required)";
            }
            cands.push(Candidate {
                freq: primary,
                channel: chans.iter().find(|c| c.freq == primary).map(|c| c.channel),
                center1: b.center,
                width,
                score,
                dfs: b.dfs,
                neighbors: nb,
                current: is_cur,
                reason,
            });
        }
        cands.sort_by(|a, b| a.score.total_cmp(&b.score).then(a.freq.cmp(&b.freq)));
        let recommend = cands.first().filter(|c| !c.current && c.score + SWITCH_MARGIN < current_score).cloned();
        cands.truncate(6);
        if cands.is_empty() && chans.is_empty() {
            plan.notes.push(format!("{}: channel list of {} unavailable, only puncturing is evaluated", r.iface, r.phy.as_deref().unwrap_or("?")));
        }
        plan.links.push(LinkPlan {
            iface: r.iface.clone(),
            link: r.link,
            band: r.band,
            freq,
            channel: r.channel,
            width,
            center1: r.center1,
            clear_mhz: clear_mhz(&bins),
            bins,
            current_score,
            puncture,
            puncture_note,
            candidates: cands,
            recommend,
        });
    }
    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bss(addr: &str, freq: u32, sig: f64) -> Bss {
        Bss { bssid: addr.into(), freq, signal: sig, ssid: "x".into(), seen_ms: Some(1000) }
    }

    #[test]
    fn kernel_puncturing_rules() {
        // 80 MHz at center 5210 (5180..5240), primary 5180 = bin 0
        assert!(valid_puncturing(80, 5210, 5180, 0));
        assert!(valid_puncturing(80, 5210, 5180, 0x8));
        assert!(valid_puncturing(80, 5210, 5180, 0x2));
        assert!(!valid_puncturing(80, 5210, 5180, 0x1)); // primary punctured
        assert!(!valid_puncturing(80, 5210, 5180, 0xa)); // two bins: not in table
        // 160: pairs allowed, 3 bins not
        assert!(valid_puncturing(160, 5250, 5180, 0xc0));
        assert!(!valid_puncturing(160, 5250, 5180, 0xe0));
        // 320 MHz: primary must stay unpunctured
        assert!(valid_puncturing(320, 6105, 6055, 0xf000));
        assert!(!valid_puncturing(320, 6105, 6055, 0x3f)); // contains bin 0..5; 6055 is bin 5
        // width without puncturing support
        assert!(!valid_puncturing(40, 5190, 5180, 0x2));
        assert!(valid_puncturing(40, 5190, 5180, 0));
    }

    #[test]
    fn bins() {
        assert_eq!(bins_of(5180, 80, Some(5210)), vec![5180, 5200, 5220, 5240]);
        assert_eq!(bins_of(6055, 320, Some(6105)).len(), 16);
        assert_eq!(bins_of(2437, 20, None), vec![2437]);
    }

    fn chans_5g() -> Vec<ChanInfo> {
        (0..8u32)
            .map(|i| ChanInfo { freq: 5180 + 20 * i, channel: 36 + 4 * i, disabled: false, radar: i >= 4 }) // 52..64 radar
            .chain((0..12u32).map(|i| ChanInfo { freq: 5500 + 20 * i, channel: 100 + 4 * i, disabled: false, radar: true }))
            .collect()
    }

    #[test]
    fn block_alignment() {
        let c = chans_5g();
        let b80 = blocks(&c, "5G", 80);
        let centers: Vec<u32> = b80.iter().map(|b| b.center).collect();
        assert_eq!(centers, vec![5210, 5290, 5530, 5610, 5690]); // 36-48, 52-64, 100-112, 116-128, 132-144
        assert_eq!(blocks(&c, "5G", 160).iter().map(|b| b.center).collect::<Vec<_>>(), vec![5250, 5570]);
        assert!(blocks(&c, "5G", 160).iter().all(|b| b.dfs || b.center == 5250) );
        // a disabled channel removes the blocks that contain it
        let mut c2 = c.clone();
        c2[1].disabled = true;
        assert_eq!(blocks(&c2, "5G", 80).iter().map(|b| b.center).collect::<Vec<_>>(), vec![5290, 5530, 5610, 5690]);
        assert_eq!(blocks(&[ChanInfo { freq: 2437, channel: 6, disabled: false, radar: false }], "2.4G", 20).len(), 1);
    }

    fn radio() -> Radio {
        Radio {
            iface: "wlan0".into(),
            phy: Some("phy0".into()),
            link: Some(0),
            band: Some("5G"),
            freq: Some(5180),
            channel: Some(36),
            width: Some(80),
            center1: Some(5210),
            addr: Some("02:00:00:00:00:01".into()),
            ..Default::default()
        }
    }

    #[test]
    fn puncture_recommended_for_busy_secondary_bin() {
        // a strong neighbor on the top 20 MHz (5240): without puncturing only
        // 40 MHz is usable, with 0x8 punctured 60 MHz are.
        let scan = ScanData {
            ts: 100.0,
            per_iface: HashMap::from([("wlan0".to_string(), vec![bss("aa:aa:aa:aa:aa:01", 5240, -60.0), bss("02:00:00:00:00:01", 5180, -30.0)])]),
        };
        let chans = HashMap::from([("phy0".to_string(), chans_5g())]);
        let survey = HashMap::new();
        let radios = [radio()];
        let p = build_plan(&PlanInput { now: 130.0, radios: &radios, scan: Some(&scan), survey: &survey, phy_chans: &chans });
        let l = &p.links[0];
        assert_eq!(p.scan_age_s, Some(30.0));
        assert_eq!(l.bins.iter().map(|b| b.busy).collect::<Vec<_>>(), vec![false, false, false, true]); // own BSS ignored
        assert_eq!(l.clear_mhz, 40);
        let pr = l.puncture.as_ref().unwrap();
        assert_eq!((pr.bitmap, pr.bitmap_hex.as_str(), pr.clear_before_mhz, pr.clear_after_mhz, pr.gain_mhz), (0x8, "0x8", 40, 60, 20));
    }
}
