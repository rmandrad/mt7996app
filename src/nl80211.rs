//! Minimal generic-netlink client for nl80211 station dumps.
//!
//! `iw` prints an MLO client as one aggregated record; the kernel however
//! sends the per-link statistics (signal, bytes, rates) in an
//! `NL80211_ATTR_MLO_LINKS` nest (see net/wireless/nl80211.c,
//! `nl80211_put_link_station_payload`). This module reads those directly.
//! Only what is needed is implemented; all structure parsing is pure and
//! unit-tested on synthetic buffers.
use crate::model::{Rate, Stats};
use std::collections::BTreeMap;
use std::io;

// netlink / genetlink
const NETLINK_GENERIC: i32 = 16;
const GENL_ID_CTRL: u16 = 0x10;
const CTRL_CMD_GETFAMILY: u8 = 3;
const CTRL_ATTR_FAMILY_ID: u16 = 1;
const CTRL_ATTR_FAMILY_NAME: u16 = 2;
const NLM_F_REQUEST: u16 = 1;
const NLM_F_DUMP: u16 = 0x300;
const NLMSG_ERROR: u16 = 2;
const NLMSG_DONE: u16 = 3;
const NLA_TYPE_MASK: u16 = 0x3fff;

// nl80211 (include/uapi/linux/nl80211.h)
const NL80211_CMD_GET_STATION: u8 = 17;
const NL80211_ATTR_IFINDEX: u16 = 3;
const NL80211_ATTR_MAC: u16 = 6;
const NL80211_ATTR_STA_INFO: u16 = 21;
const NL80211_ATTR_MLO_LINKS: u16 = 312;
const NL80211_ATTR_MLO_LINK_ID: u16 = 313;

const STA_INACTIVE_TIME: u16 = 1;
const STA_RX_BYTES: u16 = 2;
const STA_TX_BYTES: u16 = 3;
const STA_SIGNAL: u16 = 7;
const STA_TX_BITRATE: u16 = 8;
const STA_RX_PACKETS: u16 = 9;
const STA_TX_PACKETS: u16 = 10;
const STA_TX_RETRIES: u16 = 11;
const STA_TX_FAILED: u16 = 12;
const STA_SIGNAL_AVG: u16 = 13;
const STA_RX_BITRATE: u16 = 14;
const STA_CONNECTED_TIME: u16 = 16;
const STA_RX_BYTES64: u16 = 23;
const STA_TX_BYTES64: u16 = 24;
const STA_CHAIN_SIGNAL: u16 = 25;
const STA_EXPECTED_THROUGHPUT: u16 = 27;
const STA_RX_DROP_MISC: u16 = 28;
const STA_RX_DURATION: u16 = 32;
const STA_TX_DURATION: u16 = 39;
const STA_AIRTIME_WEIGHT: u16 = 40;

const RATE_BITRATE: u16 = 1;
const RATE_MCS: u16 = 2;
const RATE_40: u16 = 3;
const RATE_SHORT_GI: u16 = 4;
const RATE_BITRATE32: u16 = 5;
const RATE_VHT_MCS: u16 = 6;
const RATE_VHT_NSS: u16 = 7;
const RATE_80: u16 = 8;
const RATE_80P80: u16 = 9;
const RATE_160: u16 = 10;
const RATE_HE_MCS: u16 = 13;
const RATE_HE_NSS: u16 = 14;
const RATE_HE_GI: u16 = 15;
const RATE_320: u16 = 18;
const RATE_EHT_MCS: u16 = 19;
const RATE_EHT_NSS: u16 = 20;
const RATE_EHT_GI: u16 = 21;

// ------------------------------------------------------------ attributes --

pub struct Attrs<'a>(&'a [u8]);

impl<'a> Iterator for Attrs<'a> {
    type Item = (u16, &'a [u8]);
    fn next(&mut self) -> Option<Self::Item> {
        let b = self.0;
        if b.len() < 4 {
            return None;
        }
        let len = u16::from_ne_bytes([b[0], b[1]]) as usize;
        let ty = u16::from_ne_bytes([b[2], b[3]]) & NLA_TYPE_MASK;
        if len < 4 || len > b.len() {
            self.0 = &[];
            return None;
        }
        let payload = &b[4..len];
        self.0 = &b[((len + 3) & !3).min(b.len())..];
        Some((ty, payload))
    }
}

pub fn attrs(b: &[u8]) -> Attrs<'_> {
    Attrs(b)
}

fn put_attr(buf: &mut Vec<u8>, ty: u16, data: &[u8]) {
    buf.extend_from_slice(&((4 + data.len()) as u16).to_ne_bytes());
    buf.extend_from_slice(&ty.to_ne_bytes());
    buf.extend_from_slice(data);
    while buf.len() % 4 != 0 {
        buf.push(0);
    }
}

fn build_msg(ty: u16, flags: u16, seq: u32, cmd: u8, attrs: &[u8]) -> Vec<u8> {
    let len = 16 + 4 + attrs.len();
    let mut m = Vec::with_capacity(len);
    m.extend_from_slice(&(len as u32).to_ne_bytes());
    m.extend_from_slice(&ty.to_ne_bytes());
    m.extend_from_slice(&flags.to_ne_bytes());
    m.extend_from_slice(&seq.to_ne_bytes());
    m.extend_from_slice(&0u32.to_ne_bytes()); // pid
    m.extend_from_slice(&[cmd, 1, 0, 0]); // genlmsghdr
    m.extend_from_slice(attrs);
    m
}

fn u8_of(b: &[u8]) -> Option<u8> {
    b.first().copied()
}
fn u16_of(b: &[u8]) -> Option<u16> {
    Some(u16::from_ne_bytes(b.get(..2)?.try_into().ok()?))
}
fn u32_of(b: &[u8]) -> Option<u32> {
    Some(u32::from_ne_bytes(b.get(..4)?.try_into().ok()?))
}
fn u64_of(b: &[u8]) -> Option<u64> {
    Some(u64::from_ne_bytes(b.get(..8)?.try_into().ok()?))
}

fn mac_of(b: &[u8]) -> Option<String> {
    if b.len() < 6 {
        return None;
    }
    Some(b[..6].iter().map(|x| format!("{x:02x}")).collect::<Vec<_>>().join(":"))
}

// ---------------------------------------------------------- station data --

fn parse_rate_nest(b: &[u8]) -> Rate {
    let (mut mbps, mut mcs, mut nss, mut width) = (None, None, None, 20u32);
    let (mut gi, mut proto) = (None, "legacy");
    for (t, v) in attrs(b) {
        match t {
            RATE_BITRATE32 => mbps = u32_of(v).map(|x| x as f64 / 10.0),
            RATE_BITRATE if mbps.is_none() => mbps = u16_of(v).map(|x| x as f64 / 10.0),
            RATE_MCS => {
                mcs = u8_of(v).map(u32::from);
                proto = "n";
            }
            RATE_VHT_MCS => {
                mcs = u8_of(v).map(u32::from);
                proto = "ac";
            }
            RATE_HE_MCS => {
                mcs = u8_of(v).map(u32::from);
                proto = "ax";
            }
            RATE_EHT_MCS => {
                mcs = u8_of(v).map(u32::from);
                proto = "be";
            }
            RATE_VHT_NSS | RATE_HE_NSS | RATE_EHT_NSS => nss = u8_of(v).map(u32::from),
            RATE_HE_GI | RATE_EHT_GI => gi = u8_of(v).map(|x| x.to_string()),
            RATE_SHORT_GI => gi = Some("short".into()),
            RATE_40 => width = 40,
            RATE_80 | RATE_80P80 => width = 80,
            RATE_160 => width = 160,
            RATE_320 => width = 320,
            _ => {}
        }
    }
    let raw = format!(
        "{} MBit/s {}MHz{}{}",
        mbps.map_or("-".into(), |m| format!("{m:.1}")),
        width,
        mcs.map_or(String::new(), |m| format!(" MCS {m}")),
        nss.map_or(String::new(), |n| format!(" NSS {n}")),
    );
    Rate { raw, mbps, mcs, nss, width: Some(width), gi, proto: proto.into() }
}

/// Parse an `NL80211_ATTR_STA_INFO` nest into the model's `Stats`.
pub fn parse_sta_info(b: &[u8]) -> Stats {
    let mut s = Stats::default();
    for (t, v) in attrs(b) {
        match t {
            STA_INACTIVE_TIME => s.inactive_ms = u32_of(v).map(u64::from),
            STA_RX_BYTES if s.rx_bytes.is_none() => s.rx_bytes = u32_of(v).map(u64::from),
            STA_TX_BYTES if s.tx_bytes.is_none() => s.tx_bytes = u32_of(v).map(u64::from),
            STA_RX_BYTES64 => s.rx_bytes = u64_of(v),
            STA_TX_BYTES64 => s.tx_bytes = u64_of(v),
            STA_RX_PACKETS => s.rx_pkts = u32_of(v).map(u64::from),
            STA_TX_PACKETS => s.tx_pkts = u32_of(v).map(u64::from),
            STA_TX_RETRIES => s.tx_retries = u32_of(v).map(u64::from),
            STA_TX_FAILED => s.tx_failed = u32_of(v).map(u64::from),
            STA_RX_DROP_MISC => s.rx_drop = u64_of(v),
            STA_SIGNAL => s.signal = u8_of(v).map(|x| x as i8 as i32),
            STA_SIGNAL_AVG => s.signal_avg = u8_of(v).map(|x| x as i8 as i32),
            STA_CHAIN_SIGNAL => s.chains = attrs(v).filter_map(|(_, c)| u8_of(c)).map(|x| x as i8 as i32).collect(),
            STA_TX_BITRATE => s.tx_rate = Some(parse_rate_nest(v)),
            STA_RX_BITRATE => s.rx_rate = Some(parse_rate_nest(v)),
            STA_CONNECTED_TIME => s.connected_s = u32_of(v).map(u64::from),
            STA_EXPECTED_THROUGHPUT => s.expected_mbps = u32_of(v).map(|x| x as f64 / 1000.0),
            STA_RX_DURATION => s.rx_airtime_us = u64_of(v),
            STA_TX_DURATION => s.tx_airtime_us = u64_of(v),
            STA_AIRTIME_WEIGHT => s.airtime_weight = u16_of(v).map(u32::from),
            _ => {}
        }
    }
    s
}

#[derive(Debug, Clone)]
pub struct NlLink {
    pub id: u32,
    pub stats: Stats,
}

#[derive(Debug, Clone, Default)]
pub struct NlStation {
    pub mac: String,
    /// aggregate stats, when the kernel sent an aggregate message
    pub stats: Option<Stats>,
    pub links: Vec<NlLink>,
}

/// Parse the attributes of one GET_STATION reply message.
pub fn parse_station_attrs(b: &[u8]) -> Option<(String, Option<Stats>, Vec<NlLink>)> {
    let mut mac = None;
    let mut agg = None;
    let mut links = vec![];
    for (t, v) in attrs(b) {
        match t {
            NL80211_ATTR_MAC => mac = mac_of(v),
            NL80211_ATTR_STA_INFO => agg = Some(parse_sta_info(v)),
            NL80211_ATTR_MLO_LINKS => {
                for (_idx, link) in attrs(v) {
                    let (mut id, mut addr, mut stats) = (None, None, Stats::default());
                    for (lt, lv) in attrs(link) {
                        match lt {
                            NL80211_ATTR_MLO_LINK_ID => id = u8_of(lv).map(u32::from),
                            NL80211_ATTR_MAC => addr = mac_of(lv),
                            NL80211_ATTR_STA_INFO => stats = parse_sta_info(lv),
                            _ => {}
                        }
                    }
                    if let Some(id) = id {
                        stats.addr = addr;
                        links.push(NlLink { id, stats });
                    }
                }
            }
            _ => {}
        }
    }
    Some((mac?, agg, links))
}

/// Merge the messages of a dump (one station may span several messages).
pub fn merge_stations(msgs: impl IntoIterator<Item = (String, Option<Stats>, Vec<NlLink>)>) -> Vec<NlStation> {
    let mut m: BTreeMap<String, NlStation> = BTreeMap::new();
    for (mac, agg, links) in msgs {
        let e = m.entry(mac.clone()).or_insert_with(|| NlStation { mac, ..Default::default() });
        if agg.is_some() {
            e.stats = agg;
        }
        for l in links {
            match e.links.iter_mut().find(|x| x.id == l.id) {
                Some(x) => *x = l,
                None => e.links.push(l),
            }
        }
    }
    let mut v: Vec<NlStation> = m.into_values().collect();
    for s in &mut v {
        s.links.sort_by_key(|l| l.id);
    }
    v
}

// ---------------------------------------------------------------- socket --

pub struct Nl {
    fd: i32,
    family: u16,
    seq: u32,
}

impl Drop for Nl {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd) };
    }
}

impl Nl {
    pub fn open() -> io::Result<Nl> {
        let fd = unsafe { libc::socket(libc::AF_NETLINK, libc::SOCK_RAW | libc::SOCK_CLOEXEC, NETLINK_GENERIC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut nl = Nl { fd, family: 0, seq: 1 };
        let tv = libc::timeval { tv_sec: 3, tv_usec: 0 };
        let rc = unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_RCVTIMEO,
                &tv as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::timeval>() as u32,
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        nl.family = nl.resolve_family("nl80211")?;
        Ok(nl)
    }

    fn send(&self, msg: &[u8]) -> io::Result<()> {
        let n = unsafe { libc::send(self.fd, msg.as_ptr() as *const libc::c_void, msg.len(), 0) };
        if n < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    /// Send a request and return the payloads (after the genl header) of all
    /// data messages until NLMSG_DONE / an ack.
    fn transact(&mut self, ty: u16, flags: u16, cmd: u8, attrs: &[u8]) -> io::Result<Vec<Vec<u8>>> {
        self.seq += 1;
        let seq = self.seq;
        self.send(&build_msg(ty, flags, seq, cmd, attrs))?;
        let mut out = vec![];
        let mut buf = vec![0u8; 65536];
        loop {
            let n = unsafe { libc::recv(self.fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len(), 0) };
            if n < 0 {
                return Err(io::Error::last_os_error());
            }
            let mut b = &buf[..n as usize];
            while b.len() >= 16 {
                let len = u32_of(b).unwrap_or(0) as usize;
                let mty = u16_of(&b[4..]).unwrap_or(0);
                let mseq = u32_of(&b[8..]).unwrap_or(0);
                if len < 16 || len > b.len() {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "truncated netlink message"));
                }
                if mseq == seq {
                    match mty {
                        NLMSG_DONE => return Ok(out),
                        NLMSG_ERROR => {
                            let e = u32_of(&b[16..]).unwrap_or(0) as i32;
                            if e == 0 {
                                return Ok(out);
                            }
                            return Err(io::Error::from_raw_os_error(-e));
                        }
                        _ if len >= 20 => out.push(b[20..len].to_vec()),
                        _ => {}
                    }
                }
                b = &b[((len + 3) & !3).min(b.len())..];
            }
            if flags & NLM_F_DUMP == 0 {
                return Ok(out);
            }
        }
    }

    fn resolve_family(&mut self, name: &str) -> io::Result<u16> {
        let mut a = vec![];
        let mut n = name.as_bytes().to_vec();
        n.push(0);
        put_attr(&mut a, CTRL_ATTR_FAMILY_NAME, &n);
        let replies = self.transact(GENL_ID_CTRL, NLM_F_REQUEST, CTRL_CMD_GETFAMILY, &a)?;
        replies
            .iter()
            .flat_map(|r| attrs(r).collect::<Vec<_>>())
            .find(|(t, _)| *t == CTRL_ATTR_FAMILY_ID)
            .and_then(|(_, v)| u16_of(v))
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "nl80211 family not found (cfg80211 not loaded?)"))
    }

    /// Dump all stations of an interface, including per-link statistics.
    pub fn dump_stations(&mut self, ifindex: u32) -> io::Result<Vec<NlStation>> {
        let mut a = vec![];
        put_attr(&mut a, NL80211_ATTR_IFINDEX, &ifindex.to_ne_bytes());
        let family = self.family;
        let replies = self.transact(family, NLM_F_REQUEST | NLM_F_DUMP, NL80211_CMD_GET_STATION, &a)?;
        Ok(merge_stations(replies.iter().filter_map(|r| parse_station_attrs(r))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nest(ty: u16, inner: &[u8]) -> Vec<u8> {
        let mut b = vec![];
        put_attr(&mut b, ty | 0x8000, inner);
        b
    }

    fn rate_eht(mbps10: u32, mcs: u8, nss: u8, w: u16) -> Vec<u8> {
        let mut r = vec![];
        put_attr(&mut r, RATE_BITRATE32, &mbps10.to_ne_bytes());
        put_attr(&mut r, RATE_EHT_MCS, &[mcs]);
        put_attr(&mut r, RATE_EHT_NSS, &[nss]);
        put_attr(&mut r, RATE_EHT_GI, &[0]);
        put_attr(&mut r, w, &[]);
        r
    }

    fn sta_info(sig: i8, tx_bytes: u64, mbps10: u32, w: u16) -> Vec<u8> {
        let mut i = vec![];
        put_attr(&mut i, STA_SIGNAL, &[sig as u8]);
        put_attr(&mut i, STA_TX_BYTES, &(tx_bytes as u32).to_ne_bytes());
        put_attr(&mut i, STA_TX_BYTES64, &tx_bytes.to_ne_bytes());
        put_attr(&mut i, STA_RX_BYTES, &1234u32.to_ne_bytes());
        put_attr(&mut i, STA_TX_PACKETS, &77u32.to_ne_bytes());
        put_attr(&mut i, STA_TX_RETRIES, &5u32.to_ne_bytes());
        put_attr(&mut i, STA_TX_DURATION, &999u64.to_ne_bytes());
        put_attr(&mut i, STA_AIRTIME_WEIGHT, &256u16.to_ne_bytes());
        let mut chains = vec![];
        put_attr(&mut chains, 1, &[(-50i8) as u8]);
        put_attr(&mut chains, 2, &[(-53i8) as u8]);
        i.extend(nest(STA_CHAIN_SIGNAL, &chains));
        i.extend(nest(STA_TX_BITRATE, &rate_eht(mbps10, 11, 2, w)));
        i
    }

    const MAC: [u8; 6] = [0x10, 0x91, 0xd1, 0xf7, 0x5e, 0xbd];

    fn link_msg(id: u8, addr: [u8; 6], info: Vec<u8>) -> Vec<u8> {
        let mut l = vec![];
        put_attr(&mut l, NL80211_ATTR_MLO_LINK_ID, &[id]);
        put_attr(&mut l, NL80211_ATTR_MAC, &addr);
        l.extend(nest(NL80211_ATTR_STA_INFO, &info));
        let links = nest(NL80211_ATTR_MLO_LINKS, &nest(id as u16 + 1, &l));
        let mut m = vec![];
        put_attr(&mut m, NL80211_ATTR_IFINDEX, &17u32.to_ne_bytes());
        put_attr(&mut m, NL80211_ATTR_MAC, &MAC);
        m.extend(links);
        m
    }

    #[test]
    fn sta_info_fields() {
        let s = parse_sta_info(&sta_info(-51, 5_000_000_000, 17293, RATE_320));
        assert_eq!(s.signal, Some(-51));
        assert_eq!(s.tx_bytes, Some(5_000_000_000)); // 64-bit wins over the u32
        assert_eq!(s.rx_bytes, Some(1234));
        assert_eq!((s.tx_pkts, s.tx_retries, s.tx_airtime_us, s.airtime_weight), (Some(77), Some(5), Some(999), Some(256)));
        assert_eq!(s.chains, vec![-50, -53]);
        let r = s.tx_rate.unwrap();
        assert_eq!((r.mbps, r.mcs, r.nss, r.width, r.proto.as_str()), (Some(1729.3), Some(11), Some(2), Some(320), "be"));
    }

    #[test]
    fn mlo_links_in_separate_messages() {
        let l0 = link_msg(0, [0x12, 0x91, 0xd1, 0xf7, 0x5e, 0xc1], sta_info(-60, 1000, 11000, RATE_160));
        let l1 = link_msg(1, [0x12, 0x91, 0xd1, 0xf7, 0x5e, 0xc0], sta_info(-48, 9000, 28820, RATE_320));
        // an aggregate message without a links nest
        let mut agg = vec![];
        put_attr(&mut agg, NL80211_ATTR_MAC, &MAC);
        agg.extend(nest(NL80211_ATTR_STA_INFO, &sta_info(-48, 10000, 28820, RATE_320)));
        let st = merge_stations([&agg, &l0, &l1].iter().filter_map(|m| parse_station_attrs(m)));
        assert_eq!(st.len(), 1);
        let s = &st[0];
        assert_eq!(s.mac, "10:91:d1:f7:5e:bd");
        assert_eq!(s.stats.as_ref().unwrap().tx_bytes, Some(10000));
        assert_eq!(s.links.len(), 2);
        assert_eq!((s.links[0].id, s.links[0].stats.addr.as_deref()), (0, Some("12:91:d1:f7:5e:c1")));
        assert_eq!(s.links[0].stats.signal, Some(-60));
        assert_eq!(s.links[0].stats.tx_rate.as_ref().unwrap().width, Some(160));
        assert_eq!((s.links[1].id, s.links[1].stats.tx_bytes), (1, Some(9000)));
    }

    #[test]
    fn links_in_one_message_and_garbage() {
        let mut l = vec![];
        for id in 0..2u8 {
            let mut e = vec![];
            put_attr(&mut e, NL80211_ATTR_MLO_LINK_ID, &[id]);
            e.extend(nest(NL80211_ATTR_STA_INFO, &sta_info(-50 - id as i8, 100, 5000, RATE_80)));
            l.extend(nest(id as u16 + 1, &e));
        }
        let mut m = vec![];
        put_attr(&mut m, NL80211_ATTR_MAC, &MAC);
        m.extend(nest(NL80211_ATTR_MLO_LINKS, &l));
        let (_, _, links) = parse_station_attrs(&m).unwrap();
        assert_eq!(links.iter().map(|l| (l.id, l.stats.signal)).collect::<Vec<_>>(), vec![(0, Some(-50)), (1, Some(-51))]);
        // truncated / malformed input must not panic
        for n in 0..m.len() {
            let _ = parse_station_attrs(&m[..n]);
        }
        assert!(parse_station_attrs(&[1, 2, 3]).is_none());
    }

    #[test]
    fn request_encoding() {
        let mut a = vec![];
        put_attr(&mut a, NL80211_ATTR_IFINDEX, &17u32.to_ne_bytes());
        let m = build_msg(0x1c, NLM_F_REQUEST | NLM_F_DUMP, 5, NL80211_CMD_GET_STATION, &a);
        assert_eq!(m.len(), 16 + 4 + 8);
        assert_eq!(u32_of(&m).unwrap() as usize, m.len());
        assert_eq!(m[16], NL80211_CMD_GET_STATION);
        assert_eq!(u16_of(&m[20..]), Some(8));
        assert_eq!(u16_of(&m[22..]), Some(NL80211_ATTR_IFINDEX));
    }

    /// Needs a kernel with cfg80211 loaded: `cargo test -- --ignored`
    #[test]
    #[ignore]
    fn live_family_resolution_and_dump() {
        let mut nl = Nl::open().expect("generic netlink + nl80211 family");
        assert!(nl.family >= 0x10);
        // ifindex 1 is `lo`: not a wireless device, the kernel must answer with an error, not hang or crash
        let r = nl.dump_stations(1);
        eprintln!("dump on lo: {r:?}");
        assert!(r.is_err() || r.unwrap().is_empty());
    }
}
