//! Operating bandwidth of a neighbouring BSS from its raw information elements.
//!
//! `iw scan dump` prints the HE / EHT operation elements partly as raw hex, so
//! the width is decoded here from the binary IEs that nl80211 hands out
//! (`NL80211_BSS_INFORMATION_ELEMENTS`). Layouts follow IEEE 802.11-2020 /
//! 802.11be: HT Operation (9.4.2.56), VHT Operation (9.4.2.158), HE Operation
//! (9.4.2.249) with its 6 GHz Operation Information, EHT Operation (9.4.2.311).
//! Everything is pure and unit-tested on hand-built elements.

const EID_HT_OPERATION: u8 = 61;
const EID_VHT_OPERATION: u8 = 192;
const EID_EXTENSION: u8 = 255;
const EXT_HE_OPERATION: u8 = 36;
const EXT_EHT_OPERATION: u8 = 106;

/// Operating channel of a BSS.
#[derive(Clone, Debug, PartialEq)]
pub struct Width {
    /// total width in MHz (for 80+80: 80, with `center2` set)
    pub mhz: u32,
    pub center1: u32,
    /// second 80 MHz segment of an 80+80 BSS
    pub center2: Option<u32>,
    /// EHT disabled-subchannel (puncturing) bitmap, bit 0 = lowest 20 MHz
    pub punct: u16,
}

impl Width {
    fn new(mhz: u32, center1: u32) -> Self {
        Width { mhz, center1, center2: None, punct: 0 }
    }

    /// Centre frequencies of the 20 MHz subchannels the BSS transmits on
    /// (punctured ones are left out; only EHT BSSes have a bitmap, and no 80+80).
    pub fn bins(&self) -> Vec<u32> {
        let seg = |c: u32| (0..(self.mhz / 20).max(1)).map(move |i| c + 10 + 20 * i - self.mhz / 2);
        let all: Vec<u32> = seg(self.center1).chain(self.center2.into_iter().flat_map(seg)).collect();
        all.into_iter().enumerate().filter(|(i, _)| *i >= 16 || self.punct & (1 << i) == 0).map(|(_, f)| f).collect()
    }

    /// Does the BSS cover `freq` (its primary channel)?
    fn covers(&self, freq: u32) -> bool {
        let inside = |c: u32| (c as i64 - freq as i64).unsigned_abs() < (self.mhz / 2) as u64;
        inside(self.center1) || self.center2.is_some_and(inside)
    }
}

/// Channel number -> centre frequency in MHz, for the band of `freq`.
fn chan_freq(freq: u32, ch: u8) -> u32 {
    let ch = ch as u32;
    if freq < 3000 {
        2407 + 5 * ch
    } else if freq < 5925 {
        5000 + 5 * ch
    } else {
        5950 + 5 * ch
    }
}

/// Split an IE buffer into `(id, ext_id, payload)`; the payload of an
/// extension element starts after its extension id. A truncated tail is dropped.
fn elements(mut b: &[u8]) -> Vec<(u8, Option<u8>, &[u8])> {
    let mut out = vec![];
    while b.len() >= 2 {
        let (id, len) = (b[0], b[1] as usize);
        if b.len() < 2 + len {
            break;
        }
        let body = &b[2..2 + len];
        if id == EID_EXTENSION {
            if let Some((ext, rest)) = body.split_first() {
                out.push((id, Some(*ext), rest));
            }
        } else {
            out.push((id, None, body));
        }
        b = &b[2 + len..];
    }
    out
}

fn ht(freq: u32, d: &[u8]) -> Option<Width> {
    let (primary, flags) = (*d.first()?, *d.get(1)?);
    let _ = primary;
    let wide = flags & 0x04 != 0;
    match (flags & 0x03, wide) {
        (1, true) => Some(Width::new(40, freq + 10)),
        (3, true) => Some(Width::new(40, freq - 10)),
        _ => Some(Width::new(20, freq)),
    }
}

/// VHT Operation: channel width, CCFS0, CCFS1 (CCFS0/1 as in 802.11-2016, with
/// the older 160 MHz / 80+80 encodings of width 2 and 3 accepted too).
fn vht(freq: u32, d: &[u8]) -> Option<Width> {
    let (w, s0, s1) = (*d.first()?, *d.get(1)?, *d.get(2)?);
    let c = |n: u8| chan_freq(freq, n);
    match w {
        1 if s1 == 0 => Some(Width::new(80, c(s0))),
        1 if s0.abs_diff(s1) == 8 => Some(Width::new(160, c(s1))),
        1 | 3 => Some(Width { mhz: 80, center1: c(s0), center2: Some(c(s1)), punct: 0 }),
        2 => Some(Width::new(160, c(s0))),
        _ => None, // 0: 20 / 40 MHz, described by the HT element
    }
}

/// Width/centre from the 6 GHz Operation Information of the HE Operation element.
fn he_6ghz(freq: u32, info: &[u8]) -> Option<Width> {
    let (primary, control, s0, s1) = (*info.first()?, *info.get(1)?, *info.get(2)?, *info.get(3)?);
    let c = |n: u8| chan_freq(freq, n);
    match control & 0x03 {
        0 => Some(Width::new(20, c(primary))),
        1 => Some(Width::new(40, c(s0))),
        2 => Some(Width::new(80, c(s0))),
        _ if s1 == 0 => Some(Width::new(160, c(s0))),
        _ if s0.abs_diff(s1) == 8 => Some(Width::new(160, c(s1))),
        _ => Some(Width { mhz: 80, center1: c(s0), center2: Some(c(s1)), punct: 0 }),
    }
}

fn he(freq: u32, d: &[u8]) -> Option<Width> {
    // HE Operation Parameters (3) | BSS color (1) | basic HE-MCS and NSS set (2)
    let params = u32::from(*d.first()?) | u32::from(*d.get(1)?) << 8 | u32::from(*d.get(2)?) << 16;
    let mut off = 6;
    if params & (1 << 14) != 0 {
        off += 3; // VHT Operation Information
    }
    if params & (1 << 15) != 0 {
        off += 1; // Max Co-Hosted BSSID Indicator
    }
    if params & (1 << 17) == 0 {
        return None;
    }
    he_6ghz(freq, d.get(off..off + 5)?)
}

fn eht(freq: u32, d: &[u8]) -> Option<Width> {
    // EHT Operation Parameters (1) | basic EHT-MCS and NSS set (4) | information
    let params = *d.first()?;
    if params & 0x01 == 0 {
        return None; // no operation information (typical for 2.4 / 5 GHz)
    }
    let (control, s0, s1) = (*d.get(5)?, *d.get(6)?, *d.get(7)?);
    let c = |n: u8| chan_freq(freq, n);
    let mut w = match control & 0x07 {
        0 => Width::new(20, freq),
        1 => Width::new(40, c(s0)),
        2 => Width::new(80, c(s0)),
        // CCFS1 is the centre of the 160 / 320 MHz channel; CCFS0 the centre of
        // the 80 / 160 MHz segment holding the primary channel
        3 | 4 => {
            let mhz = if control & 0x07 == 3 { 160 } else { 320 };
            // implementations that only fill CCFS0 put the channel centre there
            Width::new(mhz, if s1 != 0 { c(s1) } else { c(s0) })
        }
        _ => return None,
    };
    if !w.covers(freq) && w.mhz >= 160 {
        w.center1 = c(s0);
    }
    if params & 0x02 != 0 {
        w.punct = u16::from_le_bytes([*d.get(8)?, *d.get(9)?]);
    }
    Some(w)
}

/// Operating bandwidth of the BSS whose primary channel is `freq`, from its
/// information elements. The most capable element wins (EHT, HE 6 GHz, VHT, HT).
/// `None` when no operation element is present or the result is implausible
/// (does not cover the primary channel).
pub fn bss_width(freq: u32, ies: &[u8]) -> Option<Width> {
    let (mut w_ht, mut w_vht, mut w_he, mut w_eht) = (None, None, None, None);
    for (id, ext, d) in elements(ies) {
        match (id, ext) {
            (EID_HT_OPERATION, None) => w_ht = ht(freq, d),
            (EID_VHT_OPERATION, None) => w_vht = vht(freq, d),
            (EID_EXTENSION, Some(EXT_HE_OPERATION)) => w_he = he(freq, d),
            (EID_EXTENSION, Some(EXT_EHT_OPERATION)) => w_eht = eht(freq, d),
            _ => {}
        }
    }
    [w_eht, w_he, w_vht, w_ht].into_iter().flatten().find(|w| w.covers(freq) || w.mhz == 20)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ie(id: u8, body: &[u8]) -> Vec<u8> {
        let mut v = vec![id, body.len() as u8];
        v.extend_from_slice(body);
        v
    }
    fn ext(eid: u8, body: &[u8]) -> Vec<u8> {
        let mut b = vec![eid];
        b.extend_from_slice(body);
        ie(255, &b)
    }
    fn cat(parts: &[Vec<u8>]) -> Vec<u8> {
        parts.concat()
    }

    #[test]
    fn ht40_above_and_below_2g() {
        // 2.4 GHz channel 1 + secondary above: 2412 and 2432
        let w = bss_width(2412, &ie(61, &[1, 0x05, 0, 0, 0])).unwrap();
        assert_eq!((w.mhz, w.center1), (40, 2422));
        assert_eq!(w.bins(), vec![2412, 2432]);
        // channel 11 + secondary below
        let w = bss_width(2462, &ie(61, &[11, 0x07, 0, 0, 0])).unwrap();
        assert_eq!((w.mhz, w.center1), (40, 2452));
        assert_eq!(w.bins(), vec![2442, 2462]);
        // secondary offset set but STA channel width bit clear: 20 MHz
        assert_eq!(bss_width(2412, &ie(61, &[1, 0x01, 0, 0, 0])).unwrap().mhz, 20);
        assert_eq!(bss_width(2412, &ie(61, &[1, 0x00, 0, 0, 0])).unwrap().bins(), vec![2412]);
    }

    #[test]
    fn vht_80_160_and_80p80() {
        // ch 44 primary (5220), 80 MHz block 36-48: CCFS0 = 42 (5210)
        let ies = cat(&[ie(61, &[44, 0x05, 0, 0, 0]), ie(192, &[1, 42, 0, 0, 0])]);
        let w = bss_width(5220, &ies).unwrap();
        assert_eq!((w.mhz, w.center1, w.center2), (80, 5210, None));
        assert_eq!(w.bins(), vec![5180, 5200, 5220, 5240]);
        // 160 MHz, 802.11-2016 encoding: CCFS0 = 42, CCFS1 = 50 (5250)
        let w = bss_width(5180, &ie(192, &[1, 42, 50, 0, 0])).unwrap();
        assert_eq!((w.mhz, w.center1), (160, 5250));
        assert_eq!(w.bins().len(), 8);
        // 160 MHz, old encoding
        let w = bss_width(5180, &ie(192, &[2, 50, 0, 0, 0])).unwrap();
        assert_eq!((w.mhz, w.center1), (160, 5250));
        // 80+80: 42 and 155
        let w = bss_width(5180, &ie(192, &[1, 42, 155, 0, 0])).unwrap();
        assert_eq!((w.mhz, w.center1, w.center2), (80, 5210, Some(5775)));
        assert_eq!(w.bins().len(), 8);
        assert!(w.bins().contains(&(5775 - 30)) && w.bins().contains(&5180));
        // VHT width 0 falls back to HT (40 MHz)
        let ies = cat(&[ie(61, &[36, 0x05, 0, 0, 0]), ie(192, &[0, 0, 0, 0, 0])]);
        assert_eq!(bss_width(5180, &ies).unwrap().mhz, 40);
    }

    #[test]
    fn he_6ghz_widths() {
        // params: 6 GHz info present (bit 17); basic MCS 2 bytes; info: primary, control, ccfs0, ccfs1, min rate
        let he6 = |control: u8, s0: u8, s1: u8| {
            let mut b = vec![0x00, 0x00, 0x02, 0x00, 0xfc, 0xff];
            b.extend_from_slice(&[5, control, s0, s1, 0]);
            ext(36, &b)
        };
        // primary ch 5 (5975), 80 MHz block ch 1-13: CCFS0 = 7 (5985)
        let w = bss_width(5975, &he6(2, 7, 0)).unwrap();
        assert_eq!((w.mhz, w.center1), (80, 5985));
        // 160 MHz: CCFS0 = 7, CCFS1 = 15 (6025)
        let w = bss_width(5975, &he6(3, 7, 15)).unwrap();
        assert_eq!((w.mhz, w.center1), (160, 6025));
        // 20 MHz
        assert_eq!(bss_width(5975, &he6(0, 5, 0)).unwrap().bins(), vec![5975]);
        // optional VHT info (bit 14) and co-hosted indicator (bit 15) shift the 6 GHz info
        let mut b = vec![0x00, 0xc0, 0x02, 0x00, 0xfc, 0xff, 1, 7, 0, 3];
        b.extend_from_slice(&[5, 2, 7, 0, 0]);
        assert_eq!(bss_width(5975, &ext(36, &b)).unwrap().center1, 5985);
        // no 6 GHz info (bit 17 clear) and nothing else: unknown
        assert_eq!(bss_width(5180, &ext(36, &[0, 0, 0, 0, 0xfc, 0xff])), None);
    }

    #[test]
    fn eht_320_with_puncturing() {
        // EHT Operation: params (bit 0 info present, bit 1 disabled bitmap present), 4 bytes basic
        // MCS/NSS, control (width), CCFS0, CCFS1, disabled subchannel bitmap
        let mk = |control: u8, s0: u8, s1: u8, punct: u16| {
            let mut b = vec![0x03, 0, 0, 0, 0, control, s0, s1];
            b.extend_from_slice(&punct.to_le_bytes());
            ext(106, &b)
        };
        // 320 MHz centred on 6105 (ch 31) = 5945..6265. The primary 6135 is in the upper 160 MHz
        // half, whose centre is 6185 (ch 47): CCFS0 = 47, CCFS1 = 31.
        let w = bss_width(6135, &mk(4, 47, 31, 0)).unwrap();
        assert_eq!((w.mhz, w.center1), (320, 6105));
        assert_eq!(w.bins().len(), 16);
        assert_eq!((w.bins()[0], w.bins()[15]), (5955, 6255));
        // bits 2 and 3 punctured: 5995 and 6015 are missing
        let w = bss_width(6135, &mk(4, 47, 31, 0b1100)).unwrap();
        assert_eq!(w.bins().len(), 14);
        assert!(!w.bins().contains(&5995) && !w.bins().contains(&6015) && w.bins().contains(&5975));
        // 160 MHz: CCFS0 = 7 (5985), CCFS1 = 15 (6025)
        let w = bss_width(5975, &mk(3, 7, 15, 0)).unwrap();
        assert_eq!((w.mhz, w.center1), (160, 6025));
        // an implementation that only fills CCFS0 (CCFS1 = 0) with the 160 MHz centre
        let w = bss_width(5975, &mk(3, 15, 0, 0)).unwrap();
        assert_eq!((w.mhz, w.center1), (160, 6025));
    }

    #[test]
    fn eht_wins_over_he_and_garbage_is_ignored() {
        let he = ext(36, &[0x00, 0x00, 0x02, 0x00, 0xfc, 0xff, 5, 2, 7, 0, 0]); // 80 MHz
        let eht = ext(106, &[0x01, 0, 0, 0, 0, 3, 7, 15]); // 160 MHz
        let w = bss_width(5975, &cat(&[he.clone(), eht])).unwrap();
        assert_eq!(w.mhz, 160);
        assert_eq!(bss_width(5975, &he).unwrap().mhz, 80);
        // truncated and empty buffers
        assert_eq!(bss_width(5180, &[]), None);
        assert_eq!(bss_width(5180, &[192, 9, 1, 42]), None);
        // a width that does not cover the primary channel is rejected
        assert_eq!(bss_width(5745, &ie(192, &[1, 42, 0, 0, 0])), None);
        // unrelated elements only
        assert_eq!(bss_width(5180, &cat(&[ie(0, b"ssid"), ie(1, &[0x8c])])), None);
    }
}
