//! Data model shared by the parsers, the collector and the JSON API.
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};

#[derive(Serialize, Clone, Default, Debug, PartialEq)]
pub struct Rate {
    pub raw: String,
    pub mbps: Option<f64>,
    pub mcs: Option<u32>,
    pub nss: Option<u32>,
    pub width: Option<u32>,
    pub gi: Option<String>,
    pub proto: String,
}

/// Per-station or per-link statistics (flattened into both).
#[derive(Serialize, Clone, Default, Debug)]
pub struct Stats {
    pub addr: Option<String>,
    pub signal: Option<i32>,
    pub signal_avg: Option<i32>,
    pub chains: Vec<i32>,
    pub tx_rate: Option<Rate>,
    pub rx_rate: Option<Rate>,
    pub tx_bytes: Option<u64>,
    pub rx_bytes: Option<u64>,
    pub tx_pkts: Option<u64>,
    pub rx_pkts: Option<u64>,
    pub tx_retries: Option<u64>,
    pub tx_failed: Option<u64>,
    pub rx_drop: Option<u64>,
    pub beacon_loss: Option<u64>,
    pub inactive_ms: Option<u64>,
    pub connected_s: Option<u64>,
    pub authorized: Option<bool>,
    pub expected_mbps: Option<f64>,
    pub tx_airtime_us: Option<u64>,
    pub rx_airtime_us: Option<u64>,
    // derived by the collector
    pub mbps_tx: Option<f64>,
    pub mbps_rx: Option<f64>,
    pub retry_pct: Option<f64>,
}

#[derive(Serialize, Clone, Default, Debug)]
pub struct Link {
    pub id: u32,
    pub freq: Option<u32>,
    pub band: Option<&'static str>,
    pub channel: Option<u32>,
    #[serde(flatten)]
    pub stats: Stats,
}

#[derive(Serialize, Clone, Default, Debug)]
pub struct Station {
    pub mac: String,
    pub iface: String,
    pub phy: String,
    pub mlo: bool,
    pub links: Vec<Link>,
    #[serde(flatten)]
    pub stats: Stats,
}

/// Channel/addr info of an interface, or of one link of an MLD interface.
#[derive(Clone, Default, Debug)]
pub struct IfLink {
    pub freq: Option<u32>,
    pub channel: Option<u32>,
    pub width: Option<u32>,
    pub center1: Option<u32>,
    pub center2: Option<u32>,
    pub txpower: Option<f64>,
    pub addr: Option<String>,
}

#[derive(Clone, Default, Debug)]
pub struct Iface {
    pub name: String,
    pub phy: u32,
    pub kind: Option<String>,
    pub ssid: Option<String>,
    pub ifindex: Option<u32>,
    pub base: IfLink,
    pub links: BTreeMap<u32, IfLink>,
}

impl Iface {
    #[cfg(test)]
    pub fn mlo(&self) -> bool {
        self.links.len() > 1
    }
}

#[derive(Clone, Default, Debug, PartialEq)]
pub struct SurveyChan {
    pub freq: u32,
    pub in_use: bool,
    pub noise: Option<i32>,
    pub active_ms: Option<u64>,
    pub busy_ms: Option<u64>,
    pub rx_ms: Option<u64>,
    pub tx_ms: Option<u64>,
}

#[derive(Serialize, Clone, Default, Debug, PartialEq)]
pub struct TxStats {
    pub phy: String,
    pub band: u32,
    pub attempts: Option<u64>,
    pub success: Option<u64>,
    pub per: Option<u32>,
    pub ba_miss: Option<u64>,
    pub amsdu: Vec<u64>,
    pub ampdu_len: Vec<String>,
    pub ampdu_cnt: Vec<u64>,
}

#[derive(Serialize, Clone, Default, Debug, PartialEq)]
pub struct Queue {
    pub queue: String,
    pub queued: i64,
    pub head: i64,
    pub tail: i64,
}

#[derive(Serialize, Clone, Default, Debug, PartialEq)]
pub struct Twt {
    pub wcid: u32,
    pub id: u32,
    pub flags: String,
    pub exp: i64,
    pub mantissa: i64,
    pub duration: i64,
    pub tsf: i64,
}

#[derive(Serialize, Clone, Default, Debug)]
pub struct Hw {
    pub phy: String,
    pub readable: bool,
    pub tx: Vec<TxStats>,
    pub queues: Vec<Queue>,
    pub twt: Vec<Twt>,
    pub rdd: Option<String>,
    pub dfs_hw_pattern: Option<String>,
    pub sys_recovery: Option<String>,
}

/// One undigested sample from a Source.
#[derive(Default)]
pub struct Raw {
    pub ifaces: Vec<Iface>,
    pub stations: Vec<Station>,
    pub survey: HashMap<String, Vec<SurveyChan>>,
    pub hw: Vec<Hw>,
    pub errors: Vec<String>,
}

pub trait Source: Send {
    fn demo(&self) -> bool;
    fn raw(&mut self) -> Raw;
}

#[derive(Serialize, Clone, Default, Debug)]
pub struct Radio {
    pub iface: String,
    pub link: Option<u32>,
    pub ssid: Option<String>,
    pub kind: Option<String>,
    pub freq: Option<u32>,
    pub band: Option<&'static str>,
    pub channel: Option<u32>,
    pub width: Option<u32>,
    pub center1: Option<u32>,
    pub txpower: Option<f64>,
    pub addr: Option<String>,
    pub noise: Option<i32>,
    pub busy_pct: Option<f64>,
    pub tx_pct: Option<f64>,
    pub rx_pct: Option<f64>,
    pub stations: usize,
    pub history_key: Option<String>,
}

#[derive(Serialize, Clone, Default, Debug)]
pub struct State {
    pub ts: f64,
    pub host: String,
    pub demo: bool,
    pub interval: f64,
    pub errors: Vec<String>,
    pub radios: Vec<Radio>,
    pub stations: Vec<Station>,
    pub hw: Vec<Hw>,
}
