//! Parsers for `iw` text output.
//!
//! iw has no stable machine-readable format, so everything here is lenient:
//! unknown lines are ignored and missing fields stay `None`.
use crate::model::*;
use regex::Regex;
use std::sync::LazyLock;

macro_rules! re {
    ($p:expr) => {{
        static R: LazyLock<Regex> = LazyLock::new(|| Regex::new($p).unwrap());
        &*R
    }};
}

pub fn band_of(freq: Option<u32>) -> Option<&'static str> {
    freq.map(|f| {
        if f < 3000 {
            "2.4G"
        } else if f < 5925 {
            "5G"
        } else {
            "6G"
        }
    })
}

fn num<T: std::str::FromStr>(s: &str) -> Option<T> {
    s.split_whitespace().next()?.parse().ok()
}

fn fnum(s: &str) -> Option<u32> {
    s.parse::<f64>().ok().map(|f| f as u32)
}

/// Parse a 'tx bitrate' / 'rx bitrate' value.
pub fn parse_rate(s: &str) -> Rate {
    let cap = |r: &Regex| r.captures(s).and_then(|c| c[1].parse::<u32>().ok());
    let gi = if s.contains("short GI") {
        Some("short".to_string())
    } else {
        re!(r"GI (\d+)").captures(s).map(|c| c[1].to_string())
    };
    let proto = if s.contains("802.11be") || s.contains("EHT") {
        "be"
    } else if s.contains("802.11ax") || s.contains("HE-") {
        "ax"
    } else if s.contains("VHT") {
        "ac"
    } else if s.contains("MCS") {
        "n"
    } else {
        "legacy"
    };
    Rate {
        raw: s.trim().to_string(),
        mbps: re!(r"^\s*([\d.]+) MBit/s")
            .captures(s)
            .and_then(|c| c[1].parse().ok()),
        mcs: cap(re!(r"MCS (\d+)")),
        nss: cap(re!(r"NSS (\d+)")),
        width: cap(re!(r"(\d+)MHz")),
        gi,
        proto: proto.to_string(),
    }
}

/// '-45 [-46, -47] dBm' -> (Some(-45), [-46, -47])
pub fn parse_signal(s: &str) -> (Option<i32>, Vec<i32>) {
    match re!(r"^\s*(-?\d+)(?:\s*\[([-\d,\s]+)\])?").captures(s) {
        None => (None, vec![]),
        Some(c) => {
            let chains = c
                .get(2)
                .map(|m| {
                    m.as_str()
                        .split(',')
                        .filter_map(|x| x.trim().parse().ok())
                        .collect()
                })
                .unwrap_or_default();
            (c[1].parse().ok(), chains)
        }
    }
}

/// Parse `iw dev`. MLO interfaces list 'link N' sub-blocks.
pub fn parse_iw_dev(text: &str) -> Vec<Iface> {
    let mut out: Vec<Iface> = vec![];
    let mut phy = 0u32;
    let mut cur_link: Option<u32> = None;
    for line in text.lines() {
        let mut s = line.trim();
        if s.is_empty() {
            continue;
        }
        if let Some(c) = re!(r"^phy#(\d+)").captures(s) {
            phy = c[1].parse().unwrap_or(0);
            cur_link = None;
            continue;
        }
        if let Some(c) = re!(r"^Interface (\S+)").captures(s) {
            out.push(Iface {
                name: c[1].to_string(),
                phy,
                ..Default::default()
            });
            cur_link = None;
            continue;
        }
        let Some(ifc) = out.last_mut() else { continue };
        if let Some(c) = re!(r"(?i)^link\s+(\d+)\b").captures(s) {
            let id: u32 = c[1].parse().unwrap_or(0);
            ifc.links.entry(id).or_default();
            cur_link = Some(id);
            s = s[c.get(0).unwrap().end()..].trim_matches(|ch| ch == ' ' || ch == ':');
            if s.is_empty() {
                continue;
            }
        }
        if let Some(c) = re!(
            r"^channel (\d+) \((\d+(?:\.\d+)?) MHz\)(?:, width: (\d+(?:\.\d+)?) MHz)?(?:, center1: (\d+(?:\.\d+)?) MHz)?(?:, center2: (\d+(?:\.\d+)?) MHz)?"
        )
        .captures(s)
        {
            let t = match cur_link {
                Some(id) => ifc.links.get_mut(&id).unwrap(),
                None => &mut ifc.base,
            };
            t.channel = c[1].parse().ok();
            t.freq = fnum(&c[2]);
            t.width = c.get(3).and_then(|m| fnum(m.as_str()));
            t.center1 = c.get(4).and_then(|m| fnum(m.as_str()));
            t.center2 = c.get(5).and_then(|m| fnum(m.as_str()));
            continue;
        }
        let (key, val) = s.split_once(' ').unwrap_or((s, ""));
        let val = val.trim();
        match key {
            "addr" => {
                let t = match cur_link {
                    Some(id) => ifc.links.get_mut(&id).unwrap(),
                    None => &mut ifc.base,
                };
                t.addr = Some(val.to_string());
            }
            "txpower" => {
                let t = match cur_link {
                    Some(id) => ifc.links.get_mut(&id).unwrap(),
                    None => &mut ifc.base,
                };
                t.txpower = num(val);
            }
            "ssid" => ifc.ssid = Some(val.to_string()),
            "type" => ifc.kind = Some(val.to_string()),
            "ifindex" => ifc.ifindex = num(val),
            _ => {}
        }
    }
    out
}

fn fill_sta(d: &mut Stats, key: &str, val: &str) {
    let key = key.trim().to_lowercase();
    let val = val.trim();
    match key.as_str() {
        "rx bytes" => d.rx_bytes = num(val),
        "rx packets" => d.rx_pkts = num(val),
        "tx bytes" => d.tx_bytes = num(val),
        "tx packets" => d.tx_pkts = num(val),
        "tx retries" => d.tx_retries = num(val),
        "tx failed" => d.tx_failed = num(val),
        "rx drop misc" => d.rx_drop = num(val),
        "beacon loss" => d.beacon_loss = num(val),
        "inactive time" => d.inactive_ms = num(val),
        "connected time" => d.connected_s = num(val),
        "signal" => {
            let (s, c) = parse_signal(val);
            d.signal = s;
            d.chains = c;
        }
        "signal avg" => d.signal_avg = parse_signal(val).0,
        "tx bitrate" => d.tx_rate = Some(parse_rate(val)),
        "rx bitrate" => d.rx_rate = Some(parse_rate(val)),
        "expected throughput" => {
            d.expected_mbps = re!(r"^([\d.]+)Mbps")
                .captures(val)
                .and_then(|c| c[1].parse().ok())
        }
        "authorized" => d.authorized = Some(val == "yes"),
        "link address" | "link addr" | "addr" => d.addr = Some(val.to_string()),
        "rx duration" => d.rx_airtime_us = num(val),
        "tx duration" => d.tx_airtime_us = num(val),
        _ => {}
    }
}

/// Parse `iw dev X station dump`. Per-link blocks go in `Station::links`.
pub fn parse_station_dump(text: &str) -> Vec<Station> {
    let mut out: Vec<Station> = vec![];
    let mut cur_link: Option<usize> = None;
    for line in text.lines() {
        if let Some(c) = re!(r"^Station ([0-9a-fA-F]{2}(?::[0-9a-fA-F]{2}){5}) \(on (\S+)\)").captures(line) {
            out.push(Station {
                mac: c[1].to_lowercase(),
                iface: c[2].to_string(),
                ..Default::default()
            });
            cur_link = None;
            continue;
        }
        let Some(sta) = out.last_mut() else { continue };
        if line.trim().is_empty() {
            continue;
        }
        if let Some(c) = re!(r"^\s*[Ll]ink\s+(\d+)\b(.*)$").captures(line) {
            let id: u32 = c[1].parse().unwrap_or(0);
            let idx = match sta.links.iter().position(|l| l.id == id) {
                Some(i) => i,
                None => {
                    sta.links.push(Link { id, ..Default::default() });
                    sta.links.len() - 1
                }
            };
            cur_link = Some(idx);
            let rest = c[2].trim_matches(|ch: char| ch == ' ' || ch == ':');
            if let Some((k, v)) = rest.split_once(':') {
                fill_sta(&mut sta.links[idx].stats, k, v);
            }
            continue;
        }
        let Some((k, v)) = line.split_once(':') else { continue };
        match cur_link {
            Some(i) if line.starts_with("\t\t") => fill_sta(&mut sta.links[i].stats, k, v),
            _ => fill_sta(&mut sta.stats, k, v),
        }
    }
    for s in &mut out {
        s.mlo = s.links.len() > 1;
    }
    out
}

/// Parse `iw dev X survey dump`.
pub fn parse_survey(text: &str) -> Vec<SurveyChan> {
    let mut out: Vec<SurveyChan> = vec![];
    for line in text.lines() {
        if let Some(c) = re!(r"^\s*frequency:\s*(\d+) MHz(\s*\[in use\])?").captures(line) {
            out.push(SurveyChan {
                freq: c[1].parse().unwrap_or(0),
                in_use: c.get(2).is_some(),
                ..Default::default()
            });
            continue;
        }
        let Some(cur) = out.last_mut() else { continue };
        let Some((k, v)) = line.split_once(':') else { continue };
        match k.trim() {
            "noise" => cur.noise = num(v),
            "channel active time" => cur.active_ms = num(v),
            "channel busy time" => cur.busy_ms = num(v),
            "channel receive time" => cur.rx_ms = num(v),
            "channel transmit time" => cur.tx_ms = num(v),
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const IW_DEV: &str = "phy#0\n\tInterface wlan0\n\t\tifindex 5\n\t\twdev 0x1\n\t\taddr 02:00:00:00:00:00\n\t\tssid lab\n\t\ttype AP\n\t\tchannel 36 (5180 MHz), width: 80 MHz, center1: 5210 MHz\n\t\ttxpower 23.00 dBm\nphy#1\n\tInterface mld0\n\t\ttype AP\n\t\tssid mlo\n\t\tlink 0 addr 02:00:00:00:00:01\n\t\t\tchannel 6 (2437 MHz), width: 40 MHz, center1: 2447 MHz\n\t\t\ttxpower 20.00 dBm\n\t\tlink 1\n\t\t\taddr 02:00:00:00:00:02\n\t\t\tchannel 5 (5975 MHz), width: 160 MHz, center1: 6025 MHz\n";

    #[test]
    fn iw_dev_plain_and_mlo() {
        let v = parse_iw_dev(IW_DEV);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].name, "wlan0");
        assert_eq!(v[0].base.freq, Some(5180));
        assert_eq!(v[0].base.width, Some(80));
        assert_eq!(v[0].base.txpower, Some(23.0));
        assert!(!v[0].mlo());
        assert!(v[1].mlo());
        assert_eq!(v[1].phy, 1);
        assert_eq!(v[1].links[&0].freq, Some(2437));
        assert_eq!(v[1].links[&0].addr.as_deref(), Some("02:00:00:00:00:01"));
        assert_eq!(v[1].links[&1].width, Some(160));
        assert_eq!(band_of(v[1].links[&1].freq), Some("6G"));
    }

    #[test]
    fn rate_parsing() {
        let r = parse_rate("2401.9 MBit/s 802.11be EHT-MCS 13 EHT-NSS 2 EHT-GI 0 ");
        assert_eq!((r.mbps, r.mcs, r.nss), (Some(2401.9), Some(13), Some(2)));
        assert_eq!(r.proto, "be");
        assert_eq!(r.gi.as_deref(), Some("0"));
        let r = parse_rate("72.2 MBit/s MCS 7 20MHz short GI");
        assert_eq!((r.proto.as_str(), r.width, r.gi.as_deref()), ("n", Some(20), Some("short")));
        assert_eq!(parse_rate("6.0 MBit/s").proto, "legacy");
    }

    #[test]
    fn signal_parsing() {
        assert_eq!(parse_signal("-45 [-46, -47] dBm"), (Some(-45), vec![-46, -47]));
        assert_eq!(parse_signal("-60 dBm"), (Some(-60), vec![]));
    }

    const STA: &str = "Station AA:BB:CC:DD:EE:FF (on wlan0)\n\tinactive time:\t40 ms\n\trx bytes:\t1000\n\ttx bytes:\t2000\n\ttx packets:\t10\n\ttx retries:\t3\n\tsignal:  \t-45 [-46, -47] dBm\n\ttx bitrate:\t866.7 MBit/s VHT-MCS 9 80MHz short GI VHT-NSS 2\n\tauthorized:\tyes\n\tconnected time:\t99 seconds\nStation 11:22:33:44:55:66 (on wlan0)\n\ttx bytes:\t5\n\t Link 0:\n\t\tsignal:\t-50 dBm\n\t\ttx bytes:\t7\n\t Link 1:\n\t\tsignal:\t-40 dBm\n\tconnected time:\t5 seconds\n";

    #[test]
    fn station_dump() {
        let v = parse_station_dump(STA);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].mac, "aa:bb:cc:dd:ee:ff");
        assert_eq!(v[0].stats.tx_bytes, Some(2000));
        assert_eq!(v[0].stats.tx_retries, Some(3));
        assert_eq!(v[0].stats.signal, Some(-45));
        assert_eq!(v[0].stats.chains, vec![-46, -47]);
        assert_eq!(v[0].stats.authorized, Some(true));
        assert_eq!(v[0].stats.tx_rate.as_ref().unwrap().proto, "ac");
        assert!(!v[0].mlo);
        assert!(v[1].mlo);
        assert_eq!(v[1].links[0].stats.signal, Some(-50));
        assert_eq!(v[1].links[0].stats.tx_bytes, Some(7));
        assert_eq!(v[1].links[1].stats.signal, Some(-40));
        // station-level keys after a link block stay on the station
        assert_eq!(v[1].stats.connected_s, Some(5));
        assert_eq!(v[1].stats.tx_bytes, Some(5));
    }

    #[test]
    fn survey_dump() {
        let t = "Survey data from wlan0\n\tfrequency:\t\t\t5180 MHz [in use]\n\tnoise:\t\t\t\t-95 dBm\n\tchannel active time:\t\t1000 ms\n\tchannel busy time:\t\t200 ms\n\tchannel receive time:\t\t50 ms\n\tchannel transmit time:\t\t30 ms\nSurvey data from wlan0\n\tfrequency:\t\t\t5200 MHz\n\tnoise:\t\t\t\t-96 dBm\n";
        let v = parse_survey(t);
        assert_eq!(v.len(), 2);
        assert!(v[0].in_use && !v[1].in_use);
        assert_eq!((v[0].noise, v[0].busy_ms, v[0].active_ms), (Some(-95), Some(200), Some(1000)));
        assert_eq!(v[1].noise, Some(-96));
    }
}
