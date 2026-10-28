//! 内存时间序列 + 低频追加落盘。
//!
//! 只存前端显示需要的精度（SPEC“数据与存储”）：
//! 档 0 原始样本留 6 小时，档 1 两分钟桶留 24 小时，档 2 十五分钟桶留 7 天，档 3 一小时桶留 30 天。
//! 每档直接由原始样本聚合。新数据先进内存的 pending，按保存间隔（默认 30 分钟）追加写盘，退出时再写一次：
//! `tN-YYYY-MM-DD.jsonl.gz` 存各档、`ev-YYYY-MM-DD.jsonl.gz` 存事件，每次追加是一个新的 gzip 成员，不重写旧内容。

use flate2::read::MultiGzDecoder;
use flate2::write::GzEncoder;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

pub struct Tier {
    /// 桶宽（秒）；0 表示原始样本
    pub step: i64,
    pub keep: i64,
}

pub const TIERS: [Tier; 4] = [
    Tier { step: 0, keep: 6 * 3600 },
    Tier { step: 120, keep: 86400 },
    Tier { step: 900, keep: 7 * 86400 },
    Tier { step: 3600, keep: 30 * 86400 },
];
pub const EVENT_KEEP: i64 = 30 * 86400;
pub const EVENT_CAP: usize = 20_000;

/// 页面时间范围对应的档。
pub fn tier_for(span: i64) -> usize {
    TIERS.iter().position(|t| span <= t.keep).unwrap_or(TIERS.len() - 1)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Agg {
    Mean,
    Min,
    Max,
    Last,
    Sum,
}

pub struct Schema {
    pub names: &'static [&'static str],
    pub aggs: &'static [Agg],
}

use Agg::*;
pub const RADIO: Schema = Schema {
    // busy_max 追加在末尾：长周期视图里保留短时尖峰
    names: &[
        "busy", "tx", "rx", "other", "self", "obss", "nf", "clients", "tx_mbps", "rx_mbps", "phy_err_ps", "tx_fail",
        "width", "chan", "busy_max",
    ],
    aggs: &[Mean, Mean, Mean, Mean, Mean, Mean, Mean, Mean, Mean, Mean, Mean, Sum, Min, Last, Max],
};
pub const VAP: Schema = Schema {
    // tx_pkts / retries 追加在末尾：旧记录少这两列，回放时补 NaN
    names: &["retry_pct", "auth_att", "auth_ok", "beacon_fail", "probe_drop", "tx_fail", "temp", "therm_level", "tx_pkts", "retries"],
    aggs: &[Mean, Sum, Sum, Sum, Sum, Sum, Max, Max, Sum, Sum],
};
pub const AP: Schema = Schema { names: &["cpu", "mem_pct", "load"], aggs: &[Mean, Mean, Mean] };
pub const CLIENT: Schema = Schema {
    names: &["rssi", "rssi_min", "tx", "rx", "idle", "band"],
    aggs: &[Mean, Min, Mean, Mean, Mean, Last],
};

pub fn schema(key: &str) -> &'static Schema {
    if key.starts_with("radio") {
        &RADIO
    } else if key.starts_with("vap") {
        &VAP
    } else if key == "ap" {
        &AP
    } else {
        &CLIENT
    }
}

fn combine(mode: Agg, acc: f64, n: u32, x: f64) -> f64 {
    if x.is_nan() {
        return acc;
    }
    if acc.is_nan() {
        return x;
    }
    match mode {
        Mean => acc + (x - acc) / (n as f64 + 1.0),
        Min => acc.min(x),
        Max => acc.max(x),
        Last => x,
        Sum => acc + x,
    }
}

/// 按固定宽度的时间桶合并样本。Mean 用逐个均值递推，NaN 视为缺失。
pub fn bucketize(rows: &[(i64, Vec<f32>)], step: i64, aggs: &[Agg]) -> Vec<(i64, Vec<f32>)> {
    let mut out: Vec<(i64, Vec<f32>)> = Vec::new();
    let mut cur: Option<(i64, Vec<f64>, Vec<u32>)> = None;
    for (t, v) in rows {
        let b = t - t.rem_euclid(step);
        match &mut cur {
            Some((cb, acc, n)) if *cb == b => {
                for i in 0..acc.len().min(v.len()) {
                    let x = v[i] as f64;
                    acc[i] = combine(aggs[i], acc[i], n[i], x);
                    if !x.is_nan() {
                        n[i] += 1;
                    }
                }
            }
            _ => {
                if let Some((cb, acc, _)) = cur.take() {
                    out.push((cb, acc.iter().map(|x| *x as f32).collect()));
                }
                let n = v.iter().map(|x| u32::from(!x.is_nan())).collect();
                cur = Some((b, v.iter().map(|x| *x as f64).collect(), n));
            }
        }
    }
    if let Some((cb, acc, _)) = cur {
        out.push((cb, acc.iter().map(|x| *x as f32).collect()));
    }
    out
}

#[derive(Default)]
pub struct Series {
    /// 各档已有的数据：档 0 是原始样本，其余是已关闭的桶
    pub rows: [VecDeque<(i64, Vec<f32>)>; 4],
    /// 档 1～3 正在累积、尚未关闭的桶里的原始样本
    open: [Vec<(i64, Vec<f32>)>; 4],
}

type Closed = Vec<(usize, i64, Vec<f32>)>;

impl Series {
    /// feed[i]=false 表示该档这一桶已经在文件里（回放时用）。返回本次关闭的桶（档, 时刻, 值）。
    fn push(&mut self, t: i64, v: Vec<f32>, now: i64, aggs: &[Agg], feed: [bool; 4]) -> Closed {
        let mut closed = Vec::new();
        for (i, tier) in TIERS.iter().enumerate().skip(1) {
            if !feed[i] {
                continue;
            }
            let b = t - t.rem_euclid(tier.step);
            if self.open[i].first().is_some_and(|(ot, _)| ot - ot.rem_euclid(tier.step) != b) {
                for (bt, bv) in bucketize(&self.open[i], tier.step, aggs) {
                    self.rows[i].push_back((bt, bv.clone()));
                    closed.push((i, bt, bv));
                }
                self.open[i].clear();
            }
            self.open[i].push((t, v.clone()));
        }
        if feed[0] && t >= now - TIERS[0].keep {
            self.rows[0].push_back((t, v));
        }
        for (i, tier) in TIERS.iter().enumerate() {
            while self.rows[i].front().is_some_and(|(x, _)| *x < now - tier.keep) {
                self.rows[i].pop_front();
            }
        }
        closed
    }

    /// 最近 secs 秒的原始样本某一列（健康规则、小图用）。
    pub fn recent(&self, field: usize, secs: i64, now: i64) -> Vec<f32> {
        self.rows[0].iter().filter(|(x, _)| *x >= now - secs).filter_map(|(_, v)| v.get(field).copied()).collect()
    }

    /// 取 [from, to] 的数据：按跨度选档，必要时再降采样到不超过 max_points 个点。
    pub fn query(&self, from: i64, to: i64, max_points: usize, aggs: &[Agg]) -> Vec<(i64, Vec<f32>)> {
        let ti = tier_for(to - from);
        let mut rows: Vec<(i64, Vec<f32>)> = self.rows[ti].iter().filter(|(t, _)| *t >= from && *t <= to).cloned().collect();
        if ti > 0 {
            // 当前未满的桶也算进去，图的右端才不会缺一段
            rows.extend(bucketize(&self.open[ti], TIERS[ti].step, aggs).into_iter().filter(|(t, _)| *t >= from && *t <= to));
        }
        if rows.len() > max_points && max_points > 0 {
            let step = ((to - from) / max_points as i64).max(1);
            rows = bucketize(&rows, step, aggs);
        }
        rows
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Event {
    pub t: i64,
    /// assoc / online / offline / roam / radar / dfs / precac / radio_restart / chan / width / ap_reboot / collector / gap / control
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub band: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub mac: Option<String>,
    pub msg: String,
    /// 来自内核日志时的 AP 开机后秒数
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub kts: Option<f64>,
}

/// 文件里的一行。字段名互不相同，untagged 才能区分。
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum Rec {
    /// 档 0 原始样本（与旧版明文文件格式相同）
    S { s: String, t: i64, v: Vec<Option<f32>> },
    /// 档 k 的已关闭桶
    B { k: usize, a: String, t: i64, v: Vec<Option<f32>> },
    E { e: Event },
}

fn enc(v: &[f32]) -> Vec<Option<f32>> {
    v.iter().map(|x| (!x.is_nan()).then_some(*x)).collect()
}

fn dec(key: &str, v: Vec<Option<f32>>) -> Vec<f32> {
    let mut v: Vec<f32> = v.into_iter().map(|x| x.unwrap_or(f32::NAN)).collect();
    v.resize(schema(key).names.len(), f32::NAN);
    v
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Name {
    pub name: String,
    pub ip: String,
}

/// 设备名册的一项：首次 / 最近一次在 AP 上见到的时刻，以及最近的频段。
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Seen {
    pub first: i64,
    pub last: i64,
    pub band: u8,
}

#[derive(Default, Serialize, Deserialize)]
struct NameFile {
    /// 2026-10-27 前经 lanwatch 拿到的主机名；只读入、并进 dhcp，不再写出
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    lan: BTreeMap<String, Name>,
    #[serde(default)]
    dhcp: BTreeMap<String, Name>,
    #[serde(default)]
    seen: BTreeMap<String, Seen>,
}

/// 设备显示名的解析结果：备注 > DHCP 主机名。IP 取租约，没有租约（静态 IP）时取 ARP 表。
#[derive(Clone, Debug, Default, Serialize, PartialEq)]
pub struct Resolved {
    pub name: Option<String>,
    /// alias / dhcp / 空
    pub name_src: &'static str,
    /// 设备自己广播的名字（DHCP 主机名），供“恢复默认”参考
    pub hostname: Option<String>,
    pub alias: Option<String>,
    pub ip: Option<String>,
}

pub struct Store {
    pub series: HashMap<String, Series>,
    pub events: VecDeque<Event>,
    pub dhcp: BTreeMap<String, Name>,
    /// 本机 ARP 表 MAC → IP，只在内存里，每轮慢速采集刷新
    pub arp: BTreeMap<String, String>,
    pub aliases: BTreeMap<String, String>,
    /// 设备名册：连过 AP 的终端，不随实时列表的 24 小时窗口过期
    pub seen: BTreeMap<String, Seen>,
    pending: [Vec<String>; 4],
    pending_ev: Vec<String>,
    dir: Option<PathBuf>,
    names_dirty: bool,
    /// 内核日志去重游标：(AP 开机时刻, 已处理到的最大时间戳)
    kstate: Option<(i64, f64)>,
    kstate_dirty: bool,
}

pub fn atomic_write(dir: &Path, name: &str, data: &[u8]) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!("{name}.tmp"));
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, dir.join(name))
}

fn append_gz(path: &Path, lines: &[String]) -> std::io::Result<()> {
    let f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    let mut g = GzEncoder::new(f, flate2::Compression::default());
    for l in lines {
        g.write_all(l.as_bytes())?;
        g.write_all(b"\n")?;
    }
    g.finish()?.sync_all()
}

/// 读压缩或明文的行文件；末尾残缺的 gzip 成员（写到一半断电）读到哪算哪。
fn read_lines(path: &Path) -> Vec<String> {
    let Ok(f) = std::fs::File::open(path) else { return Vec::new() };
    let r: Box<dyn std::io::Read> =
        if path.extension().is_some_and(|x| x == "gz") { Box::new(MultiGzDecoder::new(f)) } else { Box::new(f) };
    std::io::BufReader::new(r).lines().map_while(Result::ok).collect()
}

fn list(dir: &Path, f: impl Fn(&str) -> bool) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| rd.flatten().filter(|e| f(&e.file_name().to_string_lossy())).map(|e| e.path()).collect())
        .unwrap_or_default();
    v.sort();
    v
}

/// 2026-10-26 版本的明文日文件：YYYY-MM-DD.jsonl
fn is_legacy(n: &str) -> bool {
    n.len() == 16 && n.ends_with(".jsonl") && n.as_bytes()[4] == b'-'
}

fn file_prefix(i: usize) -> String {
    format!("t{i}-")
}

impl Store {
    pub fn new(dir: Option<PathBuf>) -> Self {
        Store {
            series: HashMap::new(),
            events: VecDeque::new(),
            dhcp: BTreeMap::new(),
            arp: BTreeMap::new(),
            aliases: BTreeMap::new(),
            seen: BTreeMap::new(),
            pending: Default::default(),
            pending_ev: Vec::new(),
            dir,
            names_dirty: false,
            kstate: None,
            kstate_dirty: false,
        }
    }

    pub fn push(&mut self, key: &str, t: i64, v: Vec<f32>, now: i64) {
        let s = schema(key);
        debug_assert_eq!(v.len(), s.names.len(), "{key}");
        if t >= now - TIERS[0].keep {
            if let Ok(l) = serde_json::to_string(&Rec::S { s: key.to_string(), t, v: enc(&v) }) {
                self.pending[0].push(l);
            }
        }
        let closed = self.series.entry(key.to_string()).or_default().push(t, v, now, s.aggs, [true; 4]);
        self.queue(key, closed);
    }

    fn queue(&mut self, key: &str, closed: Closed) {
        for (k, t, v) in closed {
            if let Ok(l) = serde_json::to_string(&Rec::B { k, a: key.to_string(), t, v: enc(&v) }) {
                self.pending[k].push(l);
            }
        }
    }

    pub fn recent(&self, key: &str, field: usize, secs: i64, now: i64) -> Vec<f32> {
        self.series.get(key).map(|s| s.recent(field, secs, now)).unwrap_or_default()
    }

    pub fn event(&mut self, e: Event) {
        if let Ok(l) = serde_json::to_string(&Rec::E { e: e.clone() }) {
            self.pending_ev.push(l);
        }
        self.insert_event(e);
    }

    fn insert_event(&mut self, e: Event) {
        // 内核日志补拉的事件可能早于已有事件，按时间插入
        let pos = self.events.iter().rposition(|x| x.t <= e.t).map(|i| i + 1).unwrap_or(0);
        self.events.insert(pos, e);
        while self.events.len() > EVENT_CAP {
            self.events.pop_front();
        }
    }

    pub fn set_arp(&mut self, list: Vec<(String, String)>) {
        self.arp = list.into_iter().collect();
    }

    pub fn set_dhcp(&mut self, mac: &str, n: Name) {
        if self.dhcp.get(mac) != Some(&n) {
            self.dhcp.insert(mac.to_string(), n);
            self.names_dirty = true;
        }
    }

    pub fn touch_seen(&mut self, mac: &str, band: u8, t: i64) {
        let e = self.seen.entry(mac.to_string()).or_insert(Seen { first: t, last: t, band });
        e.last = e.last.max(t);
        e.band = band;
        self.names_dirty = true;
    }

    /// 名册里的全部设备：在 AP 上见过的，加上只登记了备注的。
    pub fn known_macs(&self) -> Vec<String> {
        let mut v: Vec<String> = self.seen.keys().chain(self.aliases.keys()).cloned().collect();
        v.sort();
        v.dedup();
        v
    }

    /// 名册是 2026-10-27 加的：之前的设备从已有的终端序列补出首次 / 最近时刻。
    fn seed_seen(&mut self) {
        for (k, s) in &self.series {
            let Some(mac) = k.strip_prefix("c:") else { continue };
            if self.seen.contains_key(mac) {
                continue;
            }
            let first = s.rows.iter().filter_map(|r| r.front().map(|x| x.0)).min();
            let last = s.rows.iter().filter_map(|r| r.back()).max_by_key(|x| x.0);
            if let (Some(first), Some((lt, v))) = (first, last) {
                let band = v.get(5).copied().filter(|b| b.is_finite()).unwrap_or(0.0) as u8;
                self.seen.insert(mac.to_string(), Seen { first, last: *lt, band });
                self.names_dirty = true;
            }
        }
    }

    /// 2026-10-27 去掉 lanwatch 来源：它的名字本来就取自 DHCP 租约，并进 DHCP 记录，已有的 DHCP 记录优先。
    fn merge_lan(&mut self, lan: BTreeMap<String, Name>) {
        for (mac, n) in lan {
            let e = self.dhcp.entry(mac).or_default();
            if e.name.is_empty() {
                e.name = n.name;
            }
            if e.ip.is_empty() {
                e.ip = n.ip;
            }
        }
        self.dhcp.retain(|_, n| !n.name.is_empty() || !n.ip.is_empty());
        self.names_dirty = true;
    }

    /// 设置或清除自定义名。用户操作，立即写盘。
    pub fn set_alias(&mut self, mac: &str, name: Option<String>) -> std::io::Result<()> {
        match name.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
            Some(n) => self.aliases.insert(mac.to_string(), n),
            None => self.aliases.remove(mac),
        };
        if let Some(dir) = &self.dir {
            atomic_write(dir, "aliases.json", &serde_json::to_vec_pretty(&self.aliases).unwrap_or_default())?;
        }
        Ok(())
    }

    pub fn resolve(&self, mac: &str) -> Resolved {
        let alias = self.aliases.get(mac).cloned();
        let hostname = self.dhcp.get(mac).filter(|n| !n.name.is_empty()).map(|n| n.name.clone());
        let (name, name_src) = match (&alias, &hostname) {
            (Some(a), _) => (Some(a.clone()), "alias"),
            (None, Some(h)) => (Some(h.clone()), "dhcp"),
            _ => (None, ""),
        };
        let ip = self
            .dhcp
            .get(mac)
            .map(|n| n.ip.clone())
            .filter(|x| !x.is_empty())
            .or_else(|| self.arp.get(mac).cloned());
        Resolved { name, name_src, hostname, alias, ip }
    }

    /// 内核事件去重用：同一次开机内已处理到的最大时间戳；AP 重启过则从 0 开始。
    pub fn max_kts(&self, boot: i64) -> f64 {
        match self.kstate {
            Some((b, k)) if (b - boot).abs() < 120 => k,
            _ => 0.0,
        }
    }

    pub fn set_kts(&mut self, boot: i64, k: f64) {
        self.kstate = Some((boot, k));
        self.kstate_dirty = true;
    }

    /// 把 pending 追加写到当天的压缩文件。返回写入的行数。
    pub fn flush(&mut self, now: i64) -> std::io::Result<usize> {
        let Some(dir) = self.dir.clone() else {
            self.pending.iter_mut().for_each(Vec::clear);
            self.pending_ev.clear();
            return Ok(0);
        };
        std::fs::create_dir_all(&dir)?;
        let day = ymd(now);
        let mut n = 0;
        for i in 0..TIERS.len() {
            if !self.pending[i].is_empty() {
                append_gz(&dir.join(format!("{}{day}.jsonl.gz", file_prefix(i))), &self.pending[i])?;
                n += self.pending[i].len();
                self.pending[i].clear();
            }
        }
        if !self.pending_ev.is_empty() {
            append_gz(&dir.join(format!("ev-{day}.jsonl.gz")), &self.pending_ev)?;
            n += self.pending_ev.len();
            self.pending_ev.clear();
        }
        if self.names_dirty {
            let nf = NameFile { lan: BTreeMap::new(), dhcp: self.dhcp.clone(), seen: self.seen.clone() };
            atomic_write(&dir, "names.json", &serde_json::to_vec(&nf).unwrap_or_default())?;
            self.names_dirty = false;
        }
        if self.kstate_dirty {
            if let Some(k) = self.kstate {
                atomic_write(&dir, "kstate.json", &serde_json::to_vec(&k).unwrap_or_default())?;
            }
            self.kstate_dirty = false;
        }
        Ok(n)
    }

    /// 删除超过各档保留期的文件（按文件日期，多留一天）。
    pub fn prune_files(&self, now: i64) {
        let Some(dir) = &self.dir else { return };
        let mut cut: Vec<(String, String)> =
            TIERS.iter().enumerate().map(|(i, t)| (file_prefix(i), ymd(now - t.keep - 86400))).collect();
        cut.push(("ev-".into(), ymd(now - EVENT_KEEP - 86400)));
        for p in list(dir, |n| n.ends_with(".jsonl.gz")) {
            let n = p.file_name().unwrap_or_default().to_string_lossy().to_string();
            for (pre, day) in &cut {
                if let Some(d) = n.strip_prefix(pre.as_str()) {
                    if d.len() >= 10 && d[..10] < *day.as_str() {
                        let _ = std::fs::remove_file(&p);
                    }
                }
            }
        }
    }

    /// 启动时回放历史文件；遇到旧版明文日文件先迁移成新格式。返回读到的记录数。
    pub fn load(&mut self, now: i64) -> usize {
        let n = self.load_files(now);
        self.seed_seen();
        n
    }

    fn load_files(&mut self, now: i64) -> usize {
        let Some(dir) = self.dir.clone() else { return 0 };
        if let Ok(b) = std::fs::read(dir.join("names.json")) {
            if let Ok(nf) = serde_json::from_slice::<NameFile>(&b) {
                self.dhcp = nf.dhcp;
                self.seen = nf.seen;
                if !nf.lan.is_empty() {
                    self.merge_lan(nf.lan);
                }
            }
            if self.dhcp.is_empty() {
                // 2026-10-26 版 names.json 直接是 lanwatch 的 mac→名字表
                if let Ok(m) = serde_json::from_slice::<BTreeMap<String, Name>>(&b) {
                    self.merge_lan(m);
                }
            }
        }
        if let Ok(b) = std::fs::read(dir.join("aliases.json")) {
            self.aliases = serde_json::from_slice(&b).unwrap_or_default();
        }
        if let Ok(b) = std::fs::read(dir.join("kstate.json")) {
            self.kstate = serde_json::from_slice(&b).ok();
        }
        let mut n = 0;

        // 1. 旧版明文日文件：完整回放（各档都重新聚合并进 pending），写成新格式后删除
        let legacy = list(&dir, is_legacy);
        if !legacy.is_empty() {
            for p in &legacy {
                for line in read_lines(p) {
                    match serde_json::from_str::<Rec>(&line) {
                        Ok(Rec::S { s, t, v }) if t >= now - TIERS[3].keep => {
                            let v = dec(&s, v);
                            self.push(&s, t, v, now);
                            n += 1;
                        }
                        Ok(Rec::E { e }) if e.t >= now - EVENT_KEEP => {
                            self.event(e);
                            n += 1;
                        }
                        _ => {}
                    }
                }
            }
            match self.flush(now) {
                Ok(_) => legacy.iter().for_each(|p| {
                    let _ = std::fs::remove_file(p);
                }),
                Err(e) => tracing::warn!("旧数据迁移写盘失败，保留旧文件：{e}"),
            }
            return n;
        }

        // 2. 事件
        for p in list(&dir, |x| x.starts_with("ev-") && x.ends_with(".jsonl.gz")) {
            for line in read_lines(&p) {
                if let Ok(Rec::E { e }) = serde_json::from_str::<Rec>(&line) {
                    if e.t >= now - EVENT_KEEP {
                        self.insert_event(e);
                        n += 1;
                    }
                }
            }
        }
        // 3. 已关闭的桶
        #[allow(clippy::needless_range_loop)]
        for i in 1..TIERS.len() {
            for p in list(&dir, |x| x.starts_with(&file_prefix(i)) && x.ends_with(".jsonl.gz")) {
                for line in read_lines(&p) {
                    if let Ok(Rec::B { k, a, t, v }) = serde_json::from_str::<Rec>(&line) {
                        if k == i && t >= now - TIERS[i].keep {
                            let v = dec(&a, v);
                            self.series.entry(a).or_default().rows[i].push_back((t, v));
                            n += 1;
                        }
                    }
                }
            }
        }
        // 4. 原始样本：恢复档 0，并重建各档未关闭的桶；已写盘的桶不重复喂。
        //    重建时关闭的桶上次没来得及写盘（异常退出），进 pending。
        let last: HashMap<String, [Option<i64>; 4]> = self
            .series
            .iter()
            .map(|(k, s)| (k.clone(), std::array::from_fn(|i| s.rows[i].back().map(|(t, _)| *t))))
            .collect();
        for p in list(&dir, |x| x.starts_with(&file_prefix(0)) && x.ends_with(".jsonl.gz")) {
            for line in read_lines(&p) {
                if let Ok(Rec::S { s, t, v }) = serde_json::from_str::<Rec>(&line) {
                    if t < now - TIERS[0].keep {
                        continue;
                    }
                    let l = last.get(&s).copied().unwrap_or_default();
                    let feed: [bool; 4] =
                        std::array::from_fn(|i| i == 0 || l[i].is_none_or(|lt| t >= lt + TIERS[i].step));
                    let v = dec(&s, v);
                    let closed = self.series.entry(s.clone()).or_default().push(t, v, now, schema(&s).aggs, feed);
                    self.queue(&s, closed);
                    n += 1;
                }
            }
        }
        n
    }
}

pub fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// UTC+8 的日期，用作日文件名。
pub fn ymd(t: i64) -> String {
    let days = (t + 8 * 3600).div_euclid(86400);
    // 公历换算（Howard Hinnant 的 civil_from_days）
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    format!("{:04}-{:02}-{:02}", if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("apwatch-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn date_names() {
        assert_eq!(ymd(1793008784), "2026-10-26");
        assert_eq!(ymd(0), "1970-01-01");
        assert_eq!(ymd(951782400 + 16 * 3600), "2000-03-01"); // 2000-02-29 16:00 UTC = 03-01 00:00 +8
    }

    #[test]
    fn bucket_modes_and_tier_choice() {
        let rows = vec![(0, vec![1.0, 5.0, f32::NAN]), (10, vec![3.0, 2.0, 4.0]), (300, vec![9.0, 9.0, 9.0])];
        let b = bucketize(&rows, 300, &[Mean, Min, Sum]);
        assert_eq!(b.len(), 2);
        assert_eq!(b[0].1, vec![2.0, 2.0, 4.0]);
        assert_eq!(b[1].0, 300);
        assert_eq!([tier_for(3600), tier_for(6 * 3600), tier_for(86400), tier_for(7 * 86400), tier_for(30 * 86400)], [0, 0, 1, 2, 3]);
    }

    #[test]
    fn device_registry_persists_and_seeds_from_history() {
        let d = tmpdir("seen");
        let mut s = Store::new(Some(d.clone()));
        // 名册上线前就有的终端：只有序列
        for t in [NOW - 5 * 86400, NOW - 3600] {
            s.push("c:02:00:00:00:00:01", t, vec![-50.0, -55.0, 100.0, 100.0, 0.0, 1.0], NOW);
        }
        s.touch_seen("02:00:00:00:00:02", 0, NOW - 600);
        s.touch_seen("02:00:00:00:00:02", 1, NOW);
        s.set_alias("02:00:00:00:00:03", Some("还没连过的平板".into())).unwrap();
        s.flush(NOW).unwrap();
        let mut r = Store::new(Some(d.clone()));
        r.load(NOW);
        assert_eq!(r.seen["02:00:00:00:00:02"], Seen { first: NOW - 600, last: NOW, band: 1 });
        let old = r.seen["02:00:00:00:00:01"];
        assert_eq!((old.first <= NOW - 5 * 86400 + 3600, old.last >= NOW - 3600 - 900, old.band), (true, true, 1));
        assert_eq!(r.known_macs(), vec!["02:00:00:00:00:01", "02:00:00:00:00:02", "02:00:00:00:00:03"]);
        let _ = std::fs::remove_dir_all(&d);
    }

    const NOW: i64 = 1_800_000_000;

    fn fill(s: &mut Store, from: i64, to: i64) {
        let mut t = from;
        while t <= to {
            s.push("ap", t, vec![(t % 600) as f32 / 10.0, 50.0, 1.0], NOW);
            t += 30;
        }
    }

    fn views(s: &Store) -> Vec<Vec<(i64, Vec<f32>)>> {
        [3600, 6 * 3600, 86400, 7 * 86400, 30 * 86400].iter().map(|r| s.series["ap"].query(NOW - r, NOW, 100_000, AP.aggs)).collect()
    }

    #[test]
    fn tiers_keep_only_what_views_need() {
        let mut s = Store::new(None);
        fill(&mut s, NOW - 3 * 86400, NOW);
        let r = &s.series["ap"].rows;
        assert_eq!(r[0].len(), 6 * 120 + 1);
        assert!(r[1].len() <= 720 && r[1].len() >= 715, "{}", r[1].len());
        assert!(r[2].len() >= 285 && r[2].len() <= 288, "{}", r[2].len());
        assert!(r[3].len() >= 70 && r[3].len() <= 72, "{}", r[3].len());
        let v = views(&s);
        assert!(v[4].len() >= 72 && v[4].len() <= 73, "{}", v[4].len());
        assert_eq!(v[0].len(), 121);
    }

    #[test]
    fn replay_matches_memory_and_files_are_small() {
        let dir = tmpdir("replay");
        let mut s = Store::new(Some(dir.clone()));
        fill(&mut s, NOW - 3 * 86400, NOW);
        s.event(Event { t: NOW - 100, kind: "radar".into(), band: Some(1), mac: None, msg: "x".into(), kts: Some(5.0) });
        s.set_kts(NOW - 1000, 5.0);
        s.flush(NOW).unwrap();
        let mut s2 = Store::new(Some(dir.clone()));
        assert!(s2.load(NOW) > 1000);
        assert_eq!(views(&s), views(&s2));
        assert_eq!(s2.events.len(), 1);
        assert_eq!(s2.max_kts(NOW - 1000 + 30), 5.0);
        assert_eq!(s2.max_kts(NOW), 0.0);
        let size: u64 = list(&dir, |n| n.ends_with(".gz")).iter().map(|p| std::fs::metadata(p).unwrap().len()).sum();
        assert!(size < 60_000, "{size}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn crash_between_flushes_rebuilds_closed_buckets() {
        let dir = tmpdir("crash");
        let mut s = Store::new(Some(dir.clone()));
        fill(&mut s, NOW - 5 * 3600, NOW - 3600);
        s.flush(NOW).unwrap();
        // 之后又采了 50 分钟，只有原始样本落了盘（模拟桶尚未写盘时断电）
        fill(&mut s, NOW - 3570, NOW - 600);
        let raw: Vec<String> = s.pending[0].drain(..).collect();
        append_gz(&dir.join(format!("t0-{}.jsonl.gz", ymd(NOW))), &raw).unwrap();
        s.pending.iter_mut().for_each(Vec::clear);
        let mut s2 = Store::new(Some(dir.clone()));
        s2.load(NOW);
        assert_eq!(views(&s), views(&s2));
        assert!(!s2.pending[1].is_empty(), "重建的桶应进 pending 以便下次写盘");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn long_views_keep_spikes_via_max_field() {
        let mut s = Store::new(None);
        let mut t = NOW - 86400;
        while t <= NOW {
            let busy = if t % 7200 == 3600 { 95.0 } else { 10.0 };
            let mut v = vec![f32::NAN; RADIO.names.len()];
            v[0] = busy;
            v[14] = busy;
            s.push("radio1", t, v, NOW);
            t += 30;
        }
        let q = s.series["radio1"].query(NOW - 7 * 86400, NOW, 1000, RADIO.aggs);
        let max_mean = q.iter().map(|r| r.1[0]).fold(0.0f32, f32::max);
        let max_max = q.iter().map(|r| r.1[14]).fold(0.0f32, f32::max);
        assert!(max_mean < 20.0 && max_max == 95.0, "{max_mean} {max_max}");
    }

    #[test]
    fn migrates_legacy_plain_files() {
        let dir = tmpdir("legacy");
        std::fs::create_dir_all(&dir).unwrap();
        let mut lines = Vec::new();
        let mut t = NOW - 2 * 86400;
        while t <= NOW {
            lines.push(serde_json::to_string(&Rec::S { s: "ap".into(), t, v: vec![Some(10.0), Some(50.0), Some(1.0)] }).unwrap());
            t += 30;
        }
        lines.push(r#"{"e":{"t":1799999000,"kind":"radar","band":1,"msg":"x"}}"#.into());
        std::fs::write(dir.join(format!("{}.jsonl", ymd(NOW))), lines.join("\n") + "\n").unwrap();
        std::fs::write(dir.join("names.json"), r#"{"02:00:00:00:00:01":{"name":"tv","ip":"192.168.1.5"}}"#).unwrap();
        let mut s = Store::new(Some(dir.clone()));
        s.load(NOW);
        assert_eq!(s.series["ap"].rows[0].len(), 721);
        assert_eq!(s.events.len(), 1);
        assert_eq!(s.dhcp["02:00:00:00:00:01"].name, "tv");
        assert!(list(&dir, is_legacy).is_empty(), "旧文件应已删除");
        let mut s2 = Store::new(Some(dir.clone()));
        s2.load(NOW);
        assert_eq!(views(&s), views(&s2));
        assert_eq!(s2.events.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lanwatch_names_fold_into_dhcp() {
        let dir = tmpdir("lanfold");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("names.json"),
            r#"{"lan":{"02:00:00:00:00:01":{"name":"old-tv","ip":"192.168.1.5"},"02:00:00:00:00:02":{"name":"lan-name","ip":"192.168.1.6"}},
               "dhcp":{"02:00:00:00:00:02":{"name":"dhcp-name","ip":""}}}"#,
        )
        .unwrap();
        let mut s = Store::new(Some(dir.clone()));
        s.load(NOW);
        let a = s.resolve("02:00:00:00:00:01");
        assert_eq!((a.name.as_deref(), a.name_src, a.ip.as_deref()), (Some("old-tv"), "dhcp", Some("192.168.1.5")));
        let b = s.resolve("02:00:00:00:00:02");
        assert_eq!((b.name.as_deref(), b.ip.as_deref()), (Some("dhcp-name"), Some("192.168.1.6")));
        s.flush(NOW).unwrap();
        let txt = std::fs::read_to_string(dir.join("names.json")).unwrap();
        assert!(!txt.contains("\"lan\""), "合并后不再写出 lan");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_by_tier() {
        let dir = tmpdir("prune");
        std::fs::create_dir_all(&dir).unwrap();
        for f in ["t0-", "t1-", "t2-", "t3-", "ev-"] {
            for d in [1, 3, 10, 40] {
                std::fs::write(dir.join(format!("{f}{}.jsonl.gz", ymd(NOW - d * 86400))), b"").unwrap();
            }
        }
        let s = Store::new(Some(dir.clone()));
        s.prune_files(NOW);
        let left: Vec<String> = list(&dir, |_| true).iter().map(|p| p.file_name().unwrap().to_string_lossy()[..3].to_string()).collect();
        let count = |p: &str| left.iter().filter(|x| x.as_str() == p).count();
        assert_eq!((count("t0-"), count("t1-"), count("t2-"), count("t3-"), count("ev-")), (1, 1, 2, 3, 3));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn name_priority_and_alias_persist() {
        let dir = tmpdir("alias");
        let mut s = Store::new(Some(dir.clone()));
        let m = "02:00:00:00:00:01";
        assert_eq!(s.resolve(m).name, None);
        s.set_arp(vec![(m.into(), "192.168.1.5".into())]);
        assert_eq!((s.resolve(m).name, s.resolve(m).ip.as_deref()), (None, Some("192.168.1.5")));
        s.set_dhcp(m, Name { name: "tv-1".into(), ip: "192.168.1.6".into() });
        let r = s.resolve(m);
        assert_eq!((r.name.as_deref(), r.name_src, r.ip.as_deref()), (Some("tv-1"), "dhcp", Some("192.168.1.6")));
        s.set_alias(m, Some(" 客厅电视 ".into())).unwrap();
        let r = s.resolve(m);
        assert_eq!((r.name.as_deref(), r.name_src, r.hostname.as_deref()), (Some("客厅电视"), "alias", Some("tv-1")));
        let mut s2 = Store::new(Some(dir.clone()));
        s2.load(0);
        assert_eq!(s2.resolve(m).name.as_deref(), Some("客厅电视"));
        s2.set_alias(m, Some("  ".into())).unwrap();
        assert_eq!(s2.resolve(m).name_src, "");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
