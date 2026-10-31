//! In-memory event log (channel changes, joins/leaves, DFS/firmware lines
//! from the kernel log, audit trail of control actions).
use serde::Serialize;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

const CAP: usize = 500;

#[derive(Serialize, Clone, Debug)]
pub struct Event {
    pub id: u64,
    pub ts: f64,
    /// "info" | "warn" | "error"
    pub level: &'static str,
    /// "channel" | "station" | "link" | "dfs" | "firmware" | "kernel" | "control" | "system"
    pub kind: &'static str,
    pub msg: String,
}

#[derive(Default)]
struct Inner {
    q: VecDeque<Event>,
    next: u64,
}

#[derive(Default)]
pub struct EventLog {
    inner: Mutex<Inner>,
}

pub fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

impl EventLog {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn push(&self, level: &'static str, kind: &'static str, msg: impl Into<String>) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.next += 1;
        let id = g.next;
        if g.q.len() >= CAP {
            g.q.pop_front();
        }
        g.q.push_back(Event { id, ts: now(), level, kind, msg: msg.into() });
    }

    /// JSON array of events with id > `since`, oldest first.
    pub fn json(&self, since: u64) -> String {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let v: Vec<&Event> = g.q.iter().filter(|e| e.id > since).collect();
        serde_json::to_string(&v).unwrap_or_else(|_| "[]".into())
    }

    #[cfg(test)]
    pub fn all(&self) -> Vec<Event> {
        self.inner.lock().unwrap().q.iter().cloned().collect()
    }
}

/// Classify one `/dev/kmsg` record; `None` = not interesting.
/// Record format: "pri,seq,usec,flags;message".
pub fn classify_kmsg(rec: &str) -> Option<(&'static str, &'static str, String)> {
    let (head, msg) = rec.split_once(';')?;
    let msg = msg.lines().next()?.trim();
    let pri: u32 = head.split(',').next()?.parse().ok()?;
    let sev = pri & 7; // syslog level: 0 emerg .. 7 debug
    let level = if sev <= 3 { "error" } else if sev == 4 { "warn" } else { "info" };
    let m = msg.to_lowercase();
    let kind = if m.contains("radar") || m.contains("dfs") || m.contains(" cac") || m.contains("cac ") {
        "dfs"
    } else if m.contains("mt7996") || m.contains("mt76") {
        if m.contains("firmware") || m.contains("fw ") || m.contains("crash") || m.contains("timeout")
            || m.contains("recover") || m.contains("coredump") || m.contains("ser ")
        {
            "firmware"
        } else if sev <= 4 {
            "kernel"
        } else {
            return None;
        }
    } else if (m.contains("cfg80211") || m.contains("ieee80211")) && sev <= 4 {
        "kernel"
    } else {
        return None;
    };
    Some((level, kind, msg.to_string()))
}

/// Follow /dev/kmsg (needs root) and log interesting lines.
pub fn spawn_kmsg(log: Arc<EventLog>) {
    std::thread::Builder::new()
        .name("kmsg".into())
        .spawn(move || {
            use std::io::{Read, Seek, SeekFrom};
            use std::os::unix::fs::OpenOptionsExt;
            let f = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK).open("/dev/kmsg");
            let mut f = match f {
                Ok(f) => f,
                Err(e) => {
                    log.push("warn", "system", format!("kernel log not readable ({e}); DFS/firmware lines won't be logged"));
                    return;
                }
            };
            let _ = f.seek(SeekFrom::End(0)); // only new records
            let mut buf = vec![0u8; 8192];
            loop {
                match f.read(&mut buf) {
                    Ok(0) => std::thread::sleep(std::time::Duration::from_secs(1)),
                    Ok(n) => {
                        if let Some((lvl, kind, msg)) = classify_kmsg(&String::from_utf8_lossy(&buf[..n])) {
                            log.push(lvl, kind, msg);
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(std::time::Duration::from_secs(1)),
                    Err(_) => {} // EPIPE: records were overwritten, keep going
                }
            }
        })
        .ok();
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kmsg_classification() {
        let c = |s| classify_kmsg(s);
        assert_eq!(c("6,100,5000,-;mt7996e 0000:01:00.0: radar detected on 5500").unwrap().1, "dfs");
        let (lvl, kind, _) = c("3,101,5001,-;mt7996e 0000:01:00.0: Message 00020016 (seq 5) timeout").unwrap();
        assert_eq!((lvl, kind), ("error", "firmware"));
        assert_eq!(c("4,102,5002,-;mt7996e 0000:01:00.0: SER recovery state: 0x1").unwrap().1, "firmware");
        assert_eq!(c("6,103,5003,-;cfg80211: Loading compiled-in X.509 certificates").map(|x| x.1), None);
        assert_eq!(c("6,104,5004,-;usb 1-1: new device").map(|x| x.1), None);
        assert!(c("garbage").is_none());
    }

    #[test]
    fn log_ring_and_since() {
        let l = EventLog::new();
        for i in 0..(CAP + 10) {
            l.push("info", "system", format!("e{i}"));
        }
        let all = l.all();
        assert_eq!(all.len(), CAP);
        assert_eq!(all[0].msg, "e10");
        let v: Vec<serde_json::Value> = serde_json::from_str(&l.json((CAP + 8) as u64)).unwrap();
        assert_eq!(v.len(), 2);
    }
}
