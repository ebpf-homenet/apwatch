//! AP 命令输出解析。全部是纯函数，夹具在 tests/fixtures。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// `Key = value` 形式的行，键去掉首尾空白。
fn kv(text: &str) -> HashMap<String, String> {
    let mut m = HashMap::new();
    for line in text.lines() {
        if let Some((k, v)) = line.rsplit_once(" = ").or_else(|| line.split_once(" =")) {
            m.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    m
}

fn num<T: std::str::FromStr>(m: &HashMap<String, String>, k: &str) -> Option<T> {
    m.get(k)?.parse().ok()
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct RadioRaw {
    pub cycle: u32,
    pub rx_clear: u32,
    pub tx_frame: u32,
    pub rx_frame: u32,
    pub phy_err: u64,
    pub nf: i32,
    pub tx_fail: u64,
    pub tx_pkts: u64,
    pub rx_pkts: u64,
    pub self_util: Option<u32>,
    pub obss_util: Option<u32>,
    pub auth_att: u64,
    pub auth_ok: u64,
}

pub fn radio(text: &str) -> Option<RadioRaw> {
    let m = kv(text);
    let c = |k: &str| num::<u64>(&m, &format!("lithium_cycle_cnt: {k}")).map(|v| v as u32);
    Some(RadioRaw {
        cycle: c("Cycle Cnt")?,
        rx_clear: c("Rx Clear Cnt")?,
        tx_frame: c("Tx Frame Cnt")?,
        rx_frame: c("Rx Frame Cnt")?,
        phy_err: num(&m, "Rx PHY errors").unwrap_or(0),
        nf: num(&m, "lithium_cycle_cnt: Chan NF (BDF averaged NF_dBm)").unwrap_or(-95),
        tx_fail: num(&m, "Tx failures").unwrap_or(0),
        tx_pkts: num(&m, "Tx Data Packets").unwrap_or(0),
        rx_pkts: num(&m, "Rx Data Packets").unwrap_or(0),
        self_util: num(&m, "Self BSS chan util"),
        obss_util: num(&m, "OBSS chan util"),
        auth_att: num(&m, "802.11 Auth Attempts").unwrap_or(0),
        auth_ok: num(&m, "802.11 Auth Success").unwrap_or(0),
    })
}

/// 两次射频计数之差算出的信道占用（百分比）。计数是 32 位、各自回绕，用 wrapping_sub。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Busy {
    pub busy: f32,
    pub tx: f32,
    pub rx: f32,
}

pub fn busy(a: &RadioRaw, b: &RadioRaw) -> Option<Busy> {
    let cyc = b.cycle.wrapping_sub(a.cycle) as f64;
    // 30 秒约 3000 万个周期；太小说明计数被重置或采样异常，丢弃
    if cyc < 1_000_000.0 {
        return None;
    }
    let pct = |x: u32, y: u32| (y.wrapping_sub(x) as f64 / cyc * 100.0) as f32;
    let r = Busy {
        busy: pct(a.rx_clear, b.rx_clear),
        tx: pct(a.tx_frame, b.tx_frame),
        rx: pct(a.rx_frame, b.rx_frame),
    };
    (r.busy <= 101.0 && r.tx <= 101.0 && r.rx <= 101.0).then(|| Busy {
        busy: r.busy.min(100.0),
        tx: r.tx.min(100.0),
        rx: r.rx.min(100.0),
    })
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct VapRaw {
    pub tx_pkts: u64,
    pub retries: u64,
    pub tx_fail: u64,
    pub auth_att: u64,
    pub auth_ok: u64,
    pub beacon_ok: u64,
    pub beacon_fail: u64,
    pub probe_drop: u64,
}

pub fn vap(text: &str) -> Option<VapRaw> {
    let m = kv(text);
    Some(VapRaw {
        tx_pkts: num(&m, "Tx Data Packets")?,
        retries: num(&m, "Retries")?,
        tx_fail: num(&m, "Tx failures").unwrap_or(0),
        auth_att: num(&m, "802.11 Auth Attempts").unwrap_or(0),
        auth_ok: num(&m, "802.11 Auth Success").unwrap_or(0),
        beacon_ok: num(&m, "Beacon success").unwrap_or(0),
        beacon_fail: num(&m, "Beacon failed").unwrap_or(0),
        probe_drop: num(&m, "Probe request drops").unwrap_or(0),
    })
}

/// 返回（温度 °C，当前降频档位）。
pub fn therm(text: &str) -> Option<(i32, i32)> {
    let line = text.lines().find(|l| l.contains("sensor temperature:"))?;
    let t = line.split("sensor temperature:").nth(1)?.split(',').next()?.trim().parse().ok()?;
    let lv = line.split("current level:").nth(1).and_then(|s| s.trim().parse().ok()).unwrap_or(0);
    Some((t, lv))
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Sys {
    pub uptime: f64,
    pub load1: f32,
    pub date: i64,
    /// /proc/stat 首行：(忙, 总)
    pub cpu: (u64, u64),
    pub mem_total: u64,
    pub mem_avail: u64,
    /// ath0/ath1 的 (rx_bytes, tx_bytes)
    pub ifbytes: [(u64, u64); 2],
}

pub fn sys(text: &str) -> Option<Sys> {
    let mut s = Sys::default();
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    s.uptime = lines.next()?.split_whitespace().next()?.parse().ok()?;
    s.load1 = lines.next()?.split_whitespace().next()?.parse().ok()?;
    s.date = lines.next()?.parse().ok()?;
    for l in lines {
        let w: Vec<&str> = l.split_whitespace().collect();
        match w.first().copied() {
            Some("cpu") => {
                let v: Vec<u64> = w[1..].iter().filter_map(|x| x.parse().ok()).collect();
                if v.len() >= 5 {
                    let total: u64 = v.iter().sum();
                    s.cpu = (total - v[3] - v[4], total);
                }
            }
            Some("MemTotal:") => s.mem_total = w.get(1)?.parse().ok()?,
            Some("MemAvailable:") => s.mem_avail = w.get(1)?.parse().ok()?,
            Some(k) if k.starts_with("ath") => {
                // "ath0:" 后面可能直接粘着数字，按冒号切
                let (name, rest) = l.split_once(':')?;
                let n: Vec<u64> = rest.split_whitespace().filter_map(|x| x.parse().ok()).collect();
                let i = match name.trim() {
                    "ath0" => 0,
                    "ath1" => 1,
                    _ => continue,
                };
                if n.len() >= 9 {
                    s.ifbytes[i] = (n[0], n[8]);
                }
            }
            _ => {}
        }
    }
    (s.cpu.1 > 0).then_some(s)
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Iface {
    pub chan: u32,
    pub freq: u32,
    pub width: u32,
    pub center: u32,
    pub txpower: f32,
}

pub fn iw(text: &str) -> [Option<Iface>; 2] {
    let mut out = [None, None];
    let mut cur: Option<usize> = None;
    for l in text.lines().map(str::trim) {
        if let Some(name) = l.strip_prefix("Interface ") {
            cur = match name.trim() {
                "ath0" => Some(0),
                "ath1" => Some(1),
                _ => None,
            };
            if let Some(i) = cur {
                out[i] = Some(Iface::default());
            }
            continue;
        }
        let Some(f) = cur.and_then(|i| out[i].as_mut()) else { continue };
        if let Some(r) = l.strip_prefix("channel ") {
            // channel 36 (5180 MHz), width: 160 MHz, center1: 5250 MHz
            let nums: Vec<u32> = r
                .split(|c: char| !c.is_ascii_digit())
                .filter_map(|x| x.parse().ok())
                .collect();
            if nums.len() >= 3 {
                f.chan = nums[0];
                f.freq = nums[1];
                f.width = nums[2];
                f.center = nums.get(4).copied().unwrap_or(0);
            }
        } else if let Some(r) = l.strip_prefix("txpower ") {
            f.txpower = r.split_whitespace().next().and_then(|x| x.parse().ok()).unwrap_or(0.0);
        }
    }
    out
}

/// 返回两个频段的（SSID，工作模式）。
pub fn ssid(text: &str) -> [(String, String); 2] {
    let mut out: [(String, String); 2] = Default::default();
    for l in text.lines().map(str::trim) {
        let i = if l.starts_with("ath0") {
            0
        } else if l.starts_with("ath1") {
            1
        } else {
            continue;
        };
        if let Some(r) = l.split("ESSID:\"").nth(1) {
            out[i].0 = r.split('"').next().unwrap_or("").to_string();
        } else if let Some(r) = l.split("get_mode:").nth(1) {
            out[i].1 = r.trim().to_string();
        }
    }
    out
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Station {
    pub mac: String,
    pub band: u8,
    pub chan: u32,
    /// Mbps
    pub tx_rate: f32,
    pub rx_rate: f32,
    pub rssi: i32,
    pub rssi_min: i32,
    pub rssi_max: i32,
    pub idle: u32,
    /// 最高速率（kbps）
    pub max_rate: u32,
    pub assoc_secs: u64,
    pub mode: String,
    pub nss: u8,
    pub ps: bool,
}

fn rate(s: &str) -> Option<f32> {
    s.trim_end_matches('M').parse().ok()
}

fn hms(s: &str) -> Option<u64> {
    let p: Vec<u64> = s.split(':').map(|x| x.parse().ok()).collect::<Option<_>>()?;
    (p.len() == 3).then(|| p[0] * 3600 + p[1] * 60 + p[2])
}

pub fn stations(text: &str, band: u8) -> Vec<Station> {
    text.lines().filter_map(|l| station(l, band)).collect()
}

fn station(line: &str, band: u8) -> Option<Station> {
    let w: Vec<&str> = line.split_whitespace().collect();
    let mac = *w.first()?;
    if !crate::cmds::valid_mac(mac) || w.len() < 16 {
        return None;
    }
    let ai = w.iter().position(|x| x.matches(':').count() == 2 && hms(x).is_some())?;
    let mi = w.iter().position(|x| x.starts_with("IEEE80211_MODE_"))?;
    let max_rate = w[11..ai].iter().filter_map(|x| x.parse::<u32>().ok()).find(|v| *v >= 1000).unwrap_or(0);
    Some(Station {
        mac: mac.to_ascii_lowercase(),
        band,
        chan: w[2].parse().ok()?,
        tx_rate: rate(w[3])?,
        rx_rate: rate(w[4])?,
        rssi: w[5].parse().ok()?,
        rssi_min: w[6].parse().ok()?,
        rssi_max: w[7].parse().ok()?,
        idle: w[8].parse().ok()?,
        max_rate,
        assoc_secs: hms(w[ai])?,
        mode: w[mi].trim_start_matches("IEEE80211_MODE_").to_string(),
        nss: w.get(mi + 1).and_then(|x| x.parse().ok()).unwrap_or(1),
        ps: w.get(mi + 3).map(|x| *x == "1").unwrap_or(false),
    })
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct StaDetail {
    pub tx_mcs: Vec<(String, u64)>,
    pub rx_mcs: Vec<(String, u64)>,
    pub ru: Vec<(String, u64)>,
    pub stats: Vec<(String, String)>,
}

/// `apstats -s` 输出：MCS 分布只保留非零档。
pub fn sta_detail(text: &str) -> StaDetail {
    let mut d = StaDetail::default();
    let mut sec = 0;
    const KEEP: [&str; 14] = [
        "Tx Data Bytes",
        "Rx Data Bytes",
        "Tx failed",
        "Average Tx Rate (kbps)",
        "Average Rx Rate (kbps)",
        "Last tx rate",
        "Last rx rate",
        "MSDU: Tx retry count",
        "MSDU: Tx failed retry count",
        "Rx retry count",
        "Rx RSSI",
        "Band Width",
        "Last Packet Error Rate (PER)",
        "Host Discard",
    ];
    for l in text.lines() {
        let t = l.trim();
        match t {
            "TX MCS Stats" => sec = 1,
            "RX MCS Stats" => sec = 2,
            "TX OFDMA Stats" => sec = 3,
            _ => {}
        }
        if let Some((k, v)) = t.rsplit_once(':') {
            if let Ok(n) = v.trim().parse::<u64>() {
                let k = k.trim().to_string();
                let into = match sec {
                    1 if !k.contains('=') => Some(&mut d.tx_mcs),
                    2 if !k.contains('=') => Some(&mut d.rx_mcs),
                    3 if k.starts_with("RU_") => Some(&mut d.ru),
                    _ => None,
                };
                if let Some(v) = into {
                    if n > 0 || sec == 3 {
                        v.push((k, n));
                    }
                    continue;
                }
            }
        }
        if let Some((k, v)) = t.split_once('=') {
            sec = 0;
            let k = k.trim();
            if KEEP.contains(&k) {
                d.stats.push((k.to_string(), v.trim().to_string()));
            }
        }
    }
    d
}

/// 内核日志行：(开机后秒数, 文本)。续行（同一时间戳的空白行）保留，供按时间戳归组。
pub fn dmesg(text: &str) -> Vec<(f64, String)> {
    text.lines()
        .filter_map(|l| {
            let l = l.trim_end_matches('\r');
            let r = l.strip_prefix('[')?;
            let (ts, rest) = r.split_once(']')?;
            Some((ts.trim().parse().ok()?, rest.trim().to_string()))
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq)]
pub enum KEvent {
    /// 握手完成（关联/重新关联），band
    Assoc { mac: String, band: Option<u8> },
    Radar { freq: u32 },
    DfsSwitch { width: u32, chan: u32 },
    PreCacDone { freq: u32 },
    /// 射频重启（配置变更时会发生），踢掉的终端数
    RadioRestart { band: Option<u8>, clients: u32 },
}

fn band_of(s: &str) -> Option<u8> {
    if s.contains("(ath0)") || s.contains("ath0") {
        Some(0)
    } else if s.contains("(ath1)") || s.contains("ath1") {
        Some(1)
    } else {
        None
    }
}

fn first_num_after(s: &str, key: &str) -> Option<u32> {
    let r = s.split(key).nth(1)?;
    r.trim_start().split(|c: char| !c.is_ascii_digit()).find(|x| !x.is_empty())?.parse().ok()
}

/// 把同一时间戳的行归为一组后识别事件。
pub fn kevents(lines: &[(f64, String)]) -> Vec<(f64, KEvent)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let ts = lines[i].0;
        let mut j = i;
        while j < lines.len() && lines[j].0 == ts {
            j += 1;
        }
        let group = &lines[i..j];
        let vap_band = group.iter().find_map(|(_, t)| t.contains("vap-").then(|| band_of(t)).flatten());
        for (_, t) in group {
            if let Some(r) = t.split("macaddr=").nth(1).filter(|_| t.contains("set PTK")) {
                let mac = r.split(',').next().unwrap_or("").trim().to_ascii_lowercase();
                if crate::cmds::valid_mac(&mac) {
                    out.push((ts, KEvent::Assoc { mac, band: vap_band }));
                }
            } else if t.contains("Radar found") || t.contains("Radarfound") {
                out.push((ts, KEvent::Radar { freq: first_num_after(t, "Radarfound Freq=").unwrap_or(0) }));
            } else if t.contains("ieee80211_dfs_action: Changing to") {
                // Changing to HT80 non-DFS channel 40 (5200 MHz)
                let width = first_num_after(t, "Changing to HT").unwrap_or(0);
                let chan = first_num_after(t, "channel").unwrap_or(0);
                out.push((ts, KEvent::DfsSwitch { width, chan }));
            } else if t.contains("PreCAC timer Completed") {
                out.push((ts, KEvent::PreCacDone { freq: first_num_after(t, "agile freq:").unwrap_or(0) }));
            } else if t.contains("Sending bcast deauth and DELETE_ALL_PEER") {
                let clients = first_num_after(t, "clients:").unwrap_or(0);
                let band = first_num_after(t, "PSOC_").map(|p| p as u8).filter(|p| *p < 2);
                out.push((ts, KEvent::RadioRestart { band, clients }));
            }
        }
        i = j;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fx(name: &str) -> String {
        std::fs::read_to_string(format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
    }

    #[test]
    fn parses_radio_and_busy() {
        let r = radio(&fx("radio0.txt")).unwrap();
        assert_eq!(r.cycle, 3888269107);
        assert_eq!(r.rx_clear, 3863366417);
        assert_eq!(r.nf, -100);
        assert_eq!(r.phy_err, 8631680);
        assert_eq!(r.self_util, Some(9));
        assert_eq!(r.auth_att, 12696);
        // 回绕：b 的 cycle 比 a 小
        let mut r = r;
        r.cycle = u32::MAX - 1000;
        let mut b = r;
        b.cycle = r.cycle.wrapping_add(10_000_000);
        b.rx_clear = r.rx_clear.wrapping_add(1_340_000);
        b.tx_frame = r.tx_frame.wrapping_add(420_000);
        b.rx_frame = r.rx_frame.wrapping_add(870_000);
        assert!(b.cycle < r.cycle);
        let x = busy(&r, &b).unwrap();
        assert!((x.busy - 13.4).abs() < 0.01 && (x.tx - 4.2).abs() < 0.01 && (x.rx - 8.7).abs() < 0.01);
        // 计数被重置：丢弃
        assert!(busy(&r, &r).is_none());
        let mut c = b;
        c.rx_clear = r.rx_clear.wrapping_add(20_000_000);
        assert!(busy(&r, &c).is_none());
    }

    #[test]
    fn parses_vap_therm_sys() {
        let v = vap(&fx("vap1.txt")).unwrap();
        assert_eq!(v.tx_pkts, 811961985);
        assert_eq!(v.retries, 31496737);
        assert_eq!(v.beacon_fail, 27558);
        assert_eq!(v.auth_ok, 3022);
        assert_eq!(therm(&fx("therm1.txt")), Some((62, 0)));
        let s = sys(&fx("sys.txt")).unwrap();
        assert_eq!(s.date, 1793008784);
        assert!((s.uptime - 5182109.67).abs() < 0.01);
        assert_eq!(s.mem_total, 170600);
        assert_eq!(s.mem_avail, 52016);
        assert_eq!(s.cpu.1, 5093306 + 7539123 + 1003970181 + 36 + 19819281);
        assert_eq!(s.cpu.0, 5093306 + 7539123 + 19819281);
        assert_eq!(s.ifbytes[1], (62600183160, 1066042375707));
        assert_eq!(s.ifbytes[0], (20302932786, 70706114025));
    }

    #[test]
    fn parses_iw_ssid() {
        let i = iw(&fx("iw.txt"));
        let a = i[1].clone().unwrap();
        assert_eq!((a.chan, a.freq, a.width, a.center), (36, 5180, 160, 5250));
        assert_eq!(a.txpower, 33.0);
        assert_eq!(i[0].clone().unwrap().width, 40);
        let s = ssid(&fx("ssid.txt"));
        assert_eq!(s[0].0, "HomeWiFi");
        assert_eq!(s[1].0, "HomeWiFi_5G");
        assert_eq!(s[0].1, "11NGHT40PLUS");
    }

    #[test]
    fn parses_stations() {
        let s = stations(&fx("wlan0.txt"), 0);
        assert!(s.len() >= 10, "{}", s.len());
        let a = &s[0];
        assert_eq!(a.chan, 1);
        assert_eq!(a.tx_rate, 7.0);
        assert_eq!(a.rssi, -61);
        assert_eq!(a.max_rate, 86500);
        assert_eq!(a.assoc_secs, 1439 * 3600 + 22 * 60 + 49);
        assert_eq!(a.mode, "11NG_HT20");
        assert_eq!(a.nss, 1);
        let s1 = stations(&fx("wlan1.txt"), 1);
        assert!(!s1.is_empty());
        assert!(s1.iter().all(|x| x.band == 1));
        assert!(s1.iter().any(|x| x.mode == "11AXA_HE160"));
        assert!(s1.iter().any(|x| x.mode == "11AC_VHT80"));
        assert!(s1.iter().any(|x| x.max_rate == 2401900));
    }

    #[test]
    fn parses_sta_detail() {
        let d = sta_detail(&fx("sta.txt"));
        assert!(d.tx_mcs.iter().any(|(k, _)| k.starts_with("HE MCS 11")), "{:?}", d.tx_mcs);
        assert!(!d.rx_mcs.is_empty());
        assert_eq!(d.ru.len(), 6);
        assert!(d.ru.iter().any(|(k, v)| k == "RU_484" && *v == 9));
        assert!(d.stats.iter().any(|(k, v)| k == "Band Width" && v == "80"), "{:?}", d.stats);
    }

    #[test]
    fn parses_kernel_events() {
        let l = dmesg(&fx("dmesg_raw.txt"));
        assert!(l.len() > 2000);
        let ev = kevents(&l);
        let assoc = ev.iter().filter(|(_, e)| matches!(e, KEvent::Assoc { .. })).count();
        assert!(assoc > 300, "{assoc}");
        assert!(ev.iter().any(|(_, e)| matches!(e, KEvent::Assoc { band: Some(0), .. })));
        assert!(ev.iter().any(|(_, e)| matches!(e, KEvent::Assoc { band: Some(1), .. })));
        assert!(ev.iter().any(|(t, e)| *e == KEvent::Radar { freq: 5267 } && (*t - 4930271.843156).abs() < 1e-3));
        assert!(ev.iter().any(|(_, e)| *e == KEvent::DfsSwitch { width: 80, chan: 40 }));
        assert!(ev.iter().any(|(_, e)| *e == KEvent::PreCacDone { freq: 5290 }));
        assert!(ev.iter().any(|(_, e)| *e == KEvent::RadioRestart { band: Some(1), clients: 6 }));
    }
}
