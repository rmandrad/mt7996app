//! Readers/parsers for the mt7996 debugfs files.
//!
//! Layout (see drivers/net/wireless/mediatek/mt76/mt7996/debugfs.c):
//!   <root>/phyN/mt76/{tx_stats,xmit-queues,twt_stats,rdd_monitor,sys_recovery,..}
//!   <root>/phyN/netdev:IF/stations/MAC/link-N/addr
use crate::model::*;
use regex::Regex;
use std::fs;
use std::path::Path;
use std::sync::LazyLock;

pub const DEFAULT_ROOT: &str = "/sys/kernel/debug/ieee80211";
pub const SYSFS: &str = "/sys/class/ieee80211";

macro_rules! re {
    ($p:expr) => {{
        static R: LazyLock<Regex> = LazyLock::new(|| Regex::new($p).unwrap());
        &*R
    }};
}

pub fn read_text(p: impl AsRef<Path>) -> Option<String> {
    fs::read(p).ok().map(|b| String::from_utf8_lossy(&b).into_owned())
}

pub fn is_mt7996(phy: &str) -> bool {
    fs::read_link(Path::new(SYSFS).join(phy).join("device/driver"))
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().starts_with("mt7996")))
        .unwrap_or(false)
}

pub fn find_phys() -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(SYSFS)
        .map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default();
    v.retain(|p| is_mt7996(p));
    v.sort();
    v
}

fn first_num(s: &str) -> Option<u64> {
    re!(r"\d+").find(s).and_then(|m| m.as_str().parse().ok())
}

/// One entry per phy section of `tx_stats`.
pub fn parse_tx_stats(text: &str) -> Vec<TxStats> {
    let mut out: Vec<TxStats> = vec![];
    for line in text.lines() {
        if let Some(c) = re!(r"^Phy (\S+), Phy band (\d+)").captures(line) {
            out.push(TxStats {
                phy: c[1].trim_end_matches(',').to_string(),
                band: c[2].parse().unwrap_or(0),
                ..Default::default()
            });
            continue;
        }
        let Some(cur) = out.last_mut() else { continue };
        let after = |l: &str| l.split_once(':').map(|x| x.1.to_string()).unwrap_or_default();
        if line.starts_with("Length:") {
            cur.ampdu_len = after(line).split('|').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
        } else if line.starts_with("Count:") {
            cur.ampdu_cnt = re!(r"\d+").find_iter(&after(line)).filter_map(|m| m.as_str().parse().ok()).collect();
        } else if line.starts_with("BA miss count:") {
            cur.ba_miss = first_num(&after(line));
        } else if line.starts_with("Tx attempts:") {
            cur.attempts = first_num(&after(line));
        } else if line.starts_with("Tx success:") {
            cur.success = first_num(&after(line));
        } else if line.starts_with("Tx PER:") {
            cur.per = first_num(&after(line)).map(|v| v as u32);
        } else if let Some(c) = re!(r"^AMSDU pack count of (\d+) MSDU in TXD:\s*(\d+)").captures(line) {
            cur.amsdu.push(c[2].parse().unwrap_or(0));
        }
    }
    out
}

fn table_cells(line: &str) -> Vec<&str> {
    line.trim().trim_matches('|').split('|').map(str::trim).collect()
}

pub fn parse_xmit_queues(text: &str) -> Vec<Queue> {
    text.lines()
        .filter_map(|l| {
            let p = table_cells(l);
            if p.len() != 4 || p[0].is_empty() {
                return None;
            }
            Some(Queue {
                queue: p[0].to_string(),
                queued: p[1].parse().ok()?,
                head: p[2].parse().ok()?,
                tail: p[3].parse().ok()?,
            })
        })
        .collect()
}

pub fn parse_twt(text: &str) -> Vec<Twt> {
    text.lines()
        .filter_map(|l| {
            let p = table_cells(l);
            if p.len() != 7 {
                return None;
            }
            Some(Twt {
                wcid: p[0].parse().ok()?,
                id: p[1].parse().ok()?,
                flags: p[2].to_string(),
                exp: p[3].parse().ok()?,
                mantissa: p[4].parse().ok()?,
                duration: p[5].parse().ok()?,
                tsf: p[6].parse().ok()?,
            })
        })
        .collect()
}

/// Device-level debugfs info for one phy. Missing files -> empty/None.
pub fn read_hw(phy: &str, root: &str) -> Hw {
    let d = Path::new(root).join(phy).join("mt76");
    let mut hw = Hw { phy: phy.to_string(), readable: d.is_dir(), ..Default::default() };
    if !hw.readable {
        return hw;
    }
    let rd = |n: &str| read_text(d.join(n));
    let trimmed = |n: &str| rd(n).map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
    hw.tx = rd("tx_stats").map(|t| parse_tx_stats(&t)).unwrap_or_default();
    hw.queues = rd("xmit-queues").map(|t| parse_xmit_queues(&t)).unwrap_or_default();
    hw.twt = rd("twt_stats").map(|t| parse_twt(&t)).unwrap_or_default();
    hw.rdd = trimmed("rdd_monitor");
    hw.dfs_hw_pattern = trimmed("dfs_hw_pattern");
    hw.sys_recovery = trimmed("sys_recovery");
    hw
}

/// Best-effort (link id, link address) list from mac80211's station debugfs.
pub fn read_sta_links(phy: &str, iface: &str, mac: &str, root: &str) -> Vec<(u32, Option<String>)> {
    let d = Path::new(root).join(phy).join(format!("netdev:{iface}")).join("stations").join(mac);
    let mut v: Vec<(u32, Option<String>)> = fs::read_dir(&d)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().into_owned();
                    let id: u32 = name.strip_prefix("link-")?.parse().ok()?;
                    Some((id, read_text(e.path().join("addr")).map(|a| a.trim().to_string())))
                })
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    const TX: &str = "\nPhy phy0, Phy band 0\nLength:        1 |   2 -  3 |   4 -  7 | \nCount:         10 |       20 |      300 | \nBA miss count: 5\nTx attempts:    80000 (MPDUs)\nTx success:    70000 (MPDUs)\nTx PER: 12%\nTx MSDU statistics:\nAMSDU pack count of 1 MSDU in TXD:       50 (50%)\nAMSDU pack count of 2 MSDU in TXD:       50 (50%)\n\nPhy phy2, Phy band 1\nTx attempts:        0 (MPDUs)\nTx success:        0 (MPDUs)\nTx PER: 100%\n";

    #[test]
    fn tx_stats() {
        let v = parse_tx_stats(TX);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].phy, "phy0");
        assert_eq!((v[0].attempts, v[0].success, v[0].per), (Some(80000), Some(70000), Some(12)));
        assert_eq!(v[0].ba_miss, Some(5));
        assert_eq!(v[0].ampdu_cnt, vec![10, 20, 300]);
        assert_eq!(v[0].ampdu_len.len(), 3);
        assert_eq!(v[0].amsdu, vec![50, 50]);
        assert_eq!((v[1].band, v[1].per), (1, Some(100)));
    }

    #[test]
    fn queues_and_twt() {
        let q = parse_xmit_queues("     queue | hw-queued |      head |      tail |\n   MAIN0 |         3 |       100 |        97 |\n MCUFWDL |         0 |         0 |         0 |\n");
        assert_eq!(q.len(), 2);
        assert_eq!(q[0], Queue { queue: "MAIN0".into(), queued: 3, head: 100, tail: 97 });
        let t = parse_twt("     wcid |       id |    flags |      exp | mantissa | duration |            tsf |\n        3 |        1 |   sptt-a |       10 |      512 |       64 |      123456789 |\n");
        assert_eq!(t.len(), 1);
        assert_eq!((t[0].wcid, t[0].tsf, t[0].flags.as_str()), (3, 123456789, "sptt-a"));
    }
}
