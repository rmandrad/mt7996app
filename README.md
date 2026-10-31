# mt7996app

A small daemon plus web dashboard for **MediaTek mt7996 (Wi-Fi 7) access points** such as the Banana Pi BPI-R4.
It shows what MLO (multi-link operation) is actually doing and helps you tune the radios.

**Monitoring (read-only)**

- **Radios / links** - channel, width, TX power, noise floor and airtime busy/tx/rx % per link (2.4 / 5 / 6 GHz).
- **Stations** - signal, **SNR**, throughput, retry ratio and rates; MLO clients expand into a per-link breakdown with
  history graphs. Per-link numbers come straight from nl80211 (the kernel's `NL80211_ATTR_MLO_LINKS`), because `iw`
  prints an MLO client as one aggregated record.
- **Traffic history** - client throughput per band or per station as stacked bars (download up, upload below),
  with a hover tooltip, legend, and totals / average / 95th percentile / peak for the shown period, plus airtime
  busy (tx / rx / other) per link. The **Live** tab shows the last 10 min / 1 h; the **History** tab shows
  6 h to 1 year. Optionally persisted to disk, see [Traffic history](#traffic-history).
- **Hardware counters** (mt76 debugfs) - TX attempts / success / PER, BA misses, A-MSDU histogram, xmit queues, TWT flows, DFS background-radar state.
- **Event log** - channel changes, stations and MLO links joining/leaving, hostapd DFS/CAC state changes, and DFS /
  radar / firmware / mt76 lines from the kernel log.

**Advice (computed, never applied on its own)**

- **Channel and puncturing planner** - scores the neighbour BSSes of the kernel's scan cache per 20 MHz subchannel,
  recommends a channel, and recommends an EHT **preamble-puncturing** pattern when a busy secondary subchannel would
  otherwise cut the usable bandwidth. Patterns are checked against the same tables as the kernel (`net/wireless/chan.c`).
- **Airtime fairness** - spots clients that use most of the airtime while moving little data and suggests a lower
  airtime weight.
- **Band steering** - suggests 802.11v BSS-transition requests for *single-link* clients on a congested link when a
  higher band is quiet. MLO clients are never touched.

**Control (off by default)** - with `--allow-control` and a token, the UI can apply the advice (airtime weight,
`chan_switch` incl. puncturing, BSS transition request, scan). See [Control](#control-changing-radio-state).

Written in Rust (`serde`, `regex`, `tiny_http`, `libc`); the web UI is a single embedded HTML file with no external assets.

> Status: version 0.2. Verified on a BPI-R4 with a 2-link MLD AP: `iw dev`/`station dump` parsing, survey, debugfs
> panels. **Not yet verified on hardware**: the nl80211 per-link reader for a real MLO client, the planner, and every
> control action (they are unit-tested and exercised in `--demo` mode). See [Known limitations](#known-limitations).

## How it works

Every `--interval` seconds (default 2 s) the collector reads:

| Source | Used for |
|---|---|
| `iw dev` | interfaces, channels, widths, TX power, per-link info of MLD interfaces |
| `iw dev <if> station dump` | per-station and per-link counters, signal, rates |
| nl80211 (generic netlink, `NL80211_CMD_GET_STATION`) | per-link statistics of MLO clients (signal, bytes, rates, retries, airtime) |
| `iw dev <if> survey dump` | noise floor, channel busy/tx/rx airtime |
| `iw dev <if> scan dump` | neighbouring BSSes for the planner (the kernel's scan **cache**, no scan is triggered) |
| `iw phy <phy> info` | usable channels, DFS (radar) flags |
| `hostapd_cli status` (optional) | DFS / CAC state |
| `/dev/kmsg` (optional, root) | DFS, radar, firmware and mt76 messages for the event log |
| `/sys/kernel/debug/ieee80211/phyN/mt76/*` | `tx_stats`, `xmit-queues`, `twt_stats`, `rdd_monitor`, ... |
| `/sys/kernel/debug/ieee80211/phyN/netdev:IF/stations/MAC/link-N/` | per-link addresses of MLO stations |

Throughput, retry ratio and busy % are computed as deltas between samples (counter resets on re-association are
detected and skipped). The last 300 samples of each series are kept in memory and served to the UI, plus the
averaged long-term tiers for the traffic charts (see [Traffic history](#traffic-history)). Band totals
(`agg:<band>:tx|rx`) are summed from the per-link rates of MLO clients and from the station rate of single-link ones.

Only phys bound to the `mt7996e` driver are shown, unless you pass `--any-driver`.

## Requirements

- A Linux system with an mt7996 card/SoC radio running as an AP (mac80211 / mt76 driver).
- `iw` installed and in `PATH`.
- Root (or a user that can read debugfs) for the hardware panels. Without it the wireless panels still work and
  the UI shows a warning.
- debugfs mounted: `mount -t debugfs none /sys/kernel/debug` (usually already mounted).
- To build: Rust 1.80+ (`rustup`).

## Try it without hardware

```sh
cargo run --release -- --demo
# open http://127.0.0.1:8996/
```

Demo mode simulates an AP with a 3-link MLD (2.4 / 5 / 6 GHz), an MLO phone, a Wi-Fi 6 laptop and a 2.4 GHz IoT device.
The charts start empty and fill as the daemon runs; add `--history-file /tmp/demo-history.json` to see the history
survive a restart.

## The dashboard

Two tabs (the choice is kept in the URL: `#live` / `#history`):

- **Live** - radios / links, the 10 min and 1 h traffic charts, stations (click a row for per-link graphs), channel
  planner, airtime fairness, band steering, event log and the hardware panels. Updates every `--interval`.
- **History** - the same traffic and channel-load charts over **6 h, 24 h, 7 days, 30 days or 1 year**.

Both tabs share the chart controls and behaviour:

- **Traffic**: client throughput as stacked bars, *download* (AP to clients) on top and *upload* below, each with its
  own scale. Group **by band** (2.4 / 5 / 6 GHz) or **by station** (the 7 busiest stations in the period, the rest
  folded into "Other"; a station keeps its colour while it stays in the top 7). Click a legend entry to hide a series.
  Below the charts: volume, average, 95th percentile and peak for the shown period, and a per-series table.
- **Channel load**: airtime busy per link, split into this AP's TX, its RX and everything else (neighbours, noise).
  The y axis scales to the busiest bar, so read the labels.
- Hover (or focus a chart and use the arrow keys) for a tooltip with every series at that time. Dashed vertical lines
  mark channel changes and DFS events. Hatched areas mean no data was recorded (daemon not running, or not yet).
- Colours follow the light / dark setting of the browser.

## Build

```sh
git clone git@github.com:rmandrad/mt7996app.git
cd mt7996app
cargo build --release          # binary: target/release/mt7996app
cargo test                     # parser / collector unit tests
```

### Cross-compile for the BPI-R4 (aarch64)

All dependencies are pure Rust, so a static musl binary works well on OpenWrt and Debian alike.
The simplest route is [`cross`](https://github.com/cross-rs/cross) (needs Docker or Podman):

```sh
cargo install cross
cross build --release --target aarch64-unknown-linux-musl
# binary: target/aarch64-unknown-linux-musl/release/mt7996app
```

Without `cross`, install an aarch64 cross toolchain, add the target and set the linker:

```sh
rustup target add aarch64-unknown-linux-musl
mkdir -p .cargo && cat > .cargo/config.toml <<'CFG'
[target.aarch64-unknown-linux-musl]
linker = "aarch64-linux-gnu-gcc"   # or your musl cross gcc
CFG
cargo build --release --target aarch64-unknown-linux-musl
```

## Install on the router

```sh
scp target/aarch64-unknown-linux-musl/release/mt7996app root@<router>:/usr/local/bin/
ssh root@<router> 'opkg install iw || apt install iw'   # whichever your distro uses
ssh root@<router> 'mt7996app --once'                    # quick sanity check
```

`--once` samples twice and prints a text summary (add `--json` for the full state), which is the quickest way to
check that `iw` and debugfs are being read correctly.

### Run it

```sh
mt7996app                              # http://127.0.0.1:8996/ on the router
mt7996app --bind 0.0.0.0 --port 8996   # reachable from the LAN (see Security)
```

**systemd** (Debian/Armbian/Arch ARM):

```ini
# /etc/systemd/system/mt7996app.service
[Unit]
Description=mt7996 live dashboard
After=network.target hostapd.service

[Service]
ExecStart=/usr/local/bin/mt7996app --bind 127.0.0.1 --port 8996
Restart=on-failure
User=root

[Install]
WantedBy=multi-user.target
```

```sh
systemctl daemon-reload && systemctl enable --now mt7996app
```

To keep the traffic history across restarts and reboots, add `--history-file`; see
[Saving the history across restarts](#saving-the-history-across-restarts) for the full service setup.

**OpenWrt** (procd):

```sh
#!/bin/sh /etc/rc.common
# /etc/init.d/mt7996app  (chmod +x, then: /etc/init.d/mt7996app enable && start)
START=95
USE_PROCD=1
start_service() {
    procd_open_instance
    procd_set_param command /usr/local/bin/mt7996app --bind 127.0.0.1 --port 8996
    procd_set_param respawn
    procd_close_instance
}
```

### Traffic history

Throughput (per band and per station) and channel load are averaged into three tiers, kept in memory:

| Tier | Resolution | Kept for | Used for |
|---|---|---|---|
| raw | poll interval | 10 min | Live tab, 10 min |
| `1m` | 1 min | 24 h | Live 1 h, History 6 h / 24 h |
| `10m` | 10 min | 30 days | History 7 / 30 days |
| `1h` | 1 h | 1 year | History 1 year |

By default the history is lost when the daemon restarts. Pass `--history-file` to keep it:

```sh
mt7996app --history-file /var/lib/mt7996app/history.json          # saved every 15 min
mt7996app --history-file /mnt/disk/history.json --history-save 300
```

#### Saving the history across restarts

**systemd** (Debian / Armbian / Arch ARM). Same unit as above with two extra lines: the flag, and `StateDirectory=`
so systemd creates `/var/lib/mt7996app` for you.

```ini
# /etc/systemd/system/mt7996app.service
[Unit]
Description=mt7996 live dashboard
After=network.target hostapd.service

[Service]
ExecStart=/usr/local/bin/mt7996app --bind 127.0.0.1 --port 8996 --history-file /var/lib/mt7996app/history.json
StateDirectory=mt7996app
Restart=on-failure
User=root

[Install]
WantedBy=multi-user.target
```

```sh
systemctl daemon-reload
systemctl restart mt7996app            # SIGTERM: the daemon writes the file before it exits
ls -l /var/lib/mt7996app/history.json  # exists after the first save (15 min, or at the first stop)
```

To save more often, append `--history-save 300` (seconds) to `ExecStart`. `systemctl stop` and `restart` send
`SIGTERM`, which triggers a final save, so planned restarts lose nothing.

**OpenWrt** (procd). `/tmp` and `/var` are RAM there, so the file would vanish at reboot; use a USB stick or other
persistent mount (`/mnt/usb` below), and avoid flash-backed overlay paths with frequent saves:

```sh
#!/bin/sh /etc/rc.common
# /etc/init.d/mt7996app  (chmod +x, then: /etc/init.d/mt7996app enable && start)
START=95
USE_PROCD=1
start_service() {
    procd_open_instance
    procd_set_param command /usr/local/bin/mt7996app --bind 127.0.0.1 --port 8996 \
        --history-file /mnt/usb/mt7996app-history.json --history-save 900
    procd_set_param respawn
    procd_close_instance
}
```

procd stops the service with `SIGTERM` as well, so `/etc/init.d/mt7996app restart` also saves first.

**Check that it works**: after a restart, the History tab should still show the time before it, with only the
restart itself as a gap. The event log (Live tab) has a line `history restored from <file> (N series)`; the very first start has no file yet, so the line appears from the second start on. If it says
`cannot read` / `cannot save` instead, check the path's permissions and that its directory exists (`StateDirectory=`
and `mkdir -p` create it, `--history-file` does not).

The file is loaded at start, rewritten atomically (temp file + rename) every `--history-save` seconds and once more on
`SIGTERM` / `SIGINT`, so a normal stop or `systemctl restart` loses nothing. It is one JSON document with a dense array
per series (a gap is a run of `null`), around 1-2 MB when full. That is rewritten as a whole each time, so on a router
with small flash point it at tmpfs-backed or external storage and use a longer interval. Downtime shows up as an
empty stretch in the charts. Series of stations that left stay in the history until they age out of a tier.

A missing file is fine (the first run creates it); an unreadable or incompatible one is reported in the event log and
ignored, and the next save replaces it. Save failures (read-only path, disk full) are logged once and retried.
`kill -9` or a power cut loses at most `--history-save` seconds. The 10 minute raw buffer behind the Live tab's
10 min chart is never saved.

### Choosing the listen address and port

The default is `127.0.0.1:8996`. Change it with a flag or an environment variable
(command line wins over environment, environment wins over the default):

```sh
mt7996app --listen 192.168.1.1:8080       # one specific interface address
mt7996app --listen 0.0.0.0:8080           # all IPv4 interfaces
mt7996app --listen '[::]:8080'            # all IPv6 (and usually IPv4) interfaces
mt7996app --listen :8080                  # keep the default address, change the port
mt7996app --bind 192.168.1.1 --port 8080  # same as the first line

MT7996APP_LISTEN=192.168.1.1:8080 mt7996app
```

For a service, put it in the unit file, e.g. systemd:
`Environment=MT7996APP_LISTEN=192.168.1.1:8080` (or add `--listen ...` to `ExecStart`), and on OpenWrt change the
`--bind`/`--port` arguments in the procd script, or use `procd_set_param env MT7996APP_LISTEN=...`.
Only addresses that exist on the machine can be bound; the program exits with an error otherwise.

### Security

The HTTP server has **no authentication** and exposes client MAC addresses and traffic statistics. It binds to
`127.0.0.1` by default. To view it from your workstation, use an SSH tunnel instead of opening the port:

```sh
ssh -L 8996:127.0.0.1:8996 root@<router>
# then browse http://127.0.0.1:8996/
```

Only listen on `0.0.0.0` / a LAN address on a trusted management network or behind a firewall rule.

## Options

```
--listen ADDR:PORT  web server address, e.g. 0.0.0.0:8080, 192.168.1.1:8996, [::1]:8996, :8080
--bind ADDR       listen address only (default 127.0.0.1)
--port N          listen port only (default 8996)
--interval SECS   poll interval (default 2, minimum 0.2)
--demo            synthetic data, no hardware needed
--any-driver      include non-mt7996 wifi phys
--iw PATH         path to the iw binary
--debugfs PATH    debugfs ieee80211 root (default /sys/kernel/debug/ieee80211)
--once [--json]   sample twice, print a summary (or JSON) and exit
--no-netlink      do not read per-link statistics from nl80211 (iw only)
--no-kmsg         do not follow /dev/kmsg
--hostapd-cli P   hostapd_cli binary; --hostapd-ctrl DIR passes -p DIR (control socket directory)
--history-file P  persist the traffic history in this JSON file (off by default)
--history-save S  save interval in seconds (default 900, minimum 10)

--allow-control          enable POST /api/control and the auto modes (see below)
--control-token-file F   file with the shared token (>= 8 chars), or env MT7996APP_TOKEN
--control-dry-run        log the commands instead of running them
--airtime-auto           lower the airtime weight of clients hogging airtime
--steer-auto             send BSS transition requests to single-link clients
```

## HTTP API

| Endpoint | Description |
|---|---|
| `GET /` | the dashboard |
| `GET /api/state` | latest sample: `radios`, `stations` (with `links`), `hw`, `plan`, `airtime`, `steering`, `control`, `errors` |
| `GET /api/history` | time series `{ key: [[ts, value], ...] }`, e.g. `sta:<mac>:l2:tx`, `sta:<mac>:snr`, `chan:wlan0:5180:busy` |
| `GET /api/history?tier=1m\|10m\|1h` | long-term averages (24 h / 30 d / 1 y) of `agg:<band>:tx\|rx`, `sta:<mac>:tx\|rx`, `chan:<if>:<freq>:busy\|tx\|rx` |
| `GET /api/events?since=ID` | event log, oldest first; pass the last seen `id` to read incrementally |
| `POST /api/control` | JSON action with header `X-Token`; only with `--allow-control` |

## Control: changing radio state

Everything above is read-only unless you start the daemon with `--allow-control`. Then:

```sh
head -c 24 /dev/urandom | base64 > /etc/mt7996app.token && chmod 600 /etc/mt7996app.token
mt7996app --listen 127.0.0.1:8996 --allow-control --control-token-file /etc/mt7996app.token --control-dry-run
```

**Try it with `--control-dry-run` first**: every command is logged to the event log (and the UI) instead of executed.

Safety model:

- Off by default; every request needs the token in an `X-Token` header (constant-time compare, failed attempts are delayed).
  The UI asks for it once per browser session. The token is read from a file or the environment, never from `argv`.
- Actions are a fixed set of typed requests (`set_airtime`, `scan`, `chan_switch`, `btm`). All fields are validated
  (interface names, MAC addresses, ranges, puncturing bitmaps against the kernel tables) and turned into an **argv
  vector for `iw` / `hostapd_cli`** - never a shell string.
- Every executed action, manual or automatic, is written to the event log with the exact command line.
- The server has no TLS. With control enabled, keep it on `127.0.0.1` and use an SSH tunnel.
- Auto modes (`--airtime-auto`, `--steer-auto`) are separate opt-ins with hysteresis (3 consecutive samples before
  throttling, 5 calm samples before restoring; 10 minute cooldown per client for steering).

| Action | Runs |
|---|---|
| `set_airtime` | `iw dev IF station set MAC airtime_weight W` |
| `scan` | `iw dev IF scan trigger` - **the AP leaves its channel briefly** |
| `chan_switch` | `hostapd_cli [-p DIR] -i IF [-l LINK] chan_switch COUNT FREQ center_freq1=C bandwidth=B <modes> [punct_bitmap=0xN]` |
| `btm` | `hostapd_cli -i IF bss_tm_req MAC neighbor=BSSID,0x0000,OPCLASS,CHANNEL,PHYTYPE pref=1 abridged=1 valid_int=100` |

The `hostapd_cli` syntax (notably `-l LINK` for MLD links and `punct_bitmap=`) depends on your hostapd version; check
it with `hostapd_cli help` before enabling control, and test on a lab AP first.

## How the planner decides

- A 20 MHz subchannel is **busy** if a neighbouring BSS is heard on it at >= -82 dBm (the preamble-detect CCA level).
  Our own BSSes are ignored. Interference per subchannel also weighs how strong all neighbours are.
- Without puncturing, a transmission can only use the largest aligned block around the primary 20 MHz that has no busy
  subchannel. With puncturing, all non-busy subchannels stay usable - if the pattern is valid for the width.
  A puncturing recommendation needs at least +20 MHz of usable bandwidth and never punctures the primary channel.
- Channel candidates are the aligned blocks of the same width in the same band, from the phy's channel list (disabled
  channels excluded; DFS channels get a penalty because they need a channel-availability check). A switch is only
  recommended if it beats the current channel by a margin.
- Neighbours are counted on their **primary** channel only (scan results don't tell their width cheaply), and the AP
  does not scan on its own: the data is as old as the kernel's scan cache. The panel shows its age; use the
  "Trigger scan" button (control) or `hostapd` ACS to refresh it.

## Known limitations

- **Per-link MLO client data is unverified on hardware.** The nl80211 reader follows the kernel source
  (`nl80211_put_link_station_payload`) and is unit-tested on synthetic messages. If an MLO client shows no per-link
  numbers, run with `--no-netlink` to compare, and send the output of `iw dev wlan0 station dump`.
- **`iw dev` MLD output is verified** on a BPI-R4 (`MLD with links:` / `- link ID N link addr ...`). A single-link
  client on an MLD AP may be reported without link information; it is then listed as "link unknown".
- `tx_stats` covers all bands of the device. Entries are labelled with the driver's own `Phy ..., Phy band N`
  lines; the UI shows them by band index and does not map them to 2.4/5/6 GHz.
- `iw` survey data only covers channels the radio has visited, so noise/busy % is shown for the in-use channel.
- Retry % is `tx retries / (tx packets + tx retries)` from the counters, an approximation of the MPDU retry rate.
- Steering is heuristic: the client's bands are guessed from its protocol (11ac+ -> 5 GHz, 11be -> 6 GHz), and the
  neighbour report's PHY type values for HE/EHT are unverified. Treat it as a suggestion.
- Airtime control is a single conservative step (weight scaled by fair share / actual share), not a feedback loop.
- The kernel log is only followed when the daemon runs as root.
- **History is averaged and approximate.** A bar is the mean rate of its time slot (up to 1 h in the 1 year view), so
  bursts disappear, and volumes are rate x time, not byte counters. Traffic that is not on a station (broadcast,
  monitor interfaces) is not counted.
- **History is per MAC address.** Clients with randomised / rotating MACs show up as separate stations. Series of
  departed stations are kept until they age out and are not capped, so a busy public AP grows its history file.
- A band shown as "MLO (link unknown)" is traffic of an MLO client whose per-link rates were not available (see the
  nl80211 note above); it cannot be attributed to a band.
- Chart time axes use the browser's time zone and the router's clock. A router with a wrong clock (no NTP) shows
  shifted or overlapping history.

## Roadmap

- Verify and tune the control actions on hardware (hostapd `-l LINK`, BTM neighbour reports).
- Neighbour width from scan IEs for a more accurate planner; 802.11k beacon requests for steering decisions.
- History: cap the number of stored series, and a CSV / JSON export of the shown period.
- Throughput test harness (iperf3 matrix across bandwidth / MLO mode) for regression-testing mt76 changes.

## License

GPL-2.0-only, matching the Linux kernel driver it monitors.
