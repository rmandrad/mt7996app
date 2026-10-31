//! Small shared helpers.
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Run a program (no shell) with a timeout; returns stdout or a readable error.
pub fn run(prog: &str, args: &[String], timeout: Duration) -> Result<String, String> {
    let label = format!("{} {}", prog, args.join(" "));
    let mut child = Command::new(prog)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{label}: {e}"))?;
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
            Ok(None) if start.elapsed() > timeout => {
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
        let msg = if se.trim().is_empty() { status.to_string() } else { se.trim().to_string() };
        Err(format!("{label}: {msg}"))
    }
}

pub fn run_strs(prog: &str, args: &[&str], timeout: Duration) -> Result<String, String> {
    run(prog, &args.iter().map(|s| s.to_string()).collect::<Vec<_>>(), timeout)
}

pub fn is_mac(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 17
        && b.iter().enumerate().all(|(i, c)| if i % 3 == 2 { *c == b':' } else { c.is_ascii_hexdigit() })
}

pub fn is_ifname(s: &str) -> bool {
    !s.is_empty() && s.len() <= 15 && s.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'-'))
}
