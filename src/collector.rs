//! 采集调度：一个 telnet 长连接，命令串行执行。周期取自设置（默认值如下），改设置后立即重连生效。
//!
//! | 周期 | 命令 |
//! |---|---|
//! | fast_secs（30 秒） | 系统状态、信道、客户端列表 ×2、射频计数 ×2 |
//! | dmesg_secs（60 秒） | dmesg 末 40 行 |
//! | slow_secs（5 分钟） | VAP 统计 ×2、温度 ×2、SSID/模式；DHCP 租约里的主机名、本机 ARP 表里的 IP |
//! | 按需 | 单客户端 MCS 分布（10 秒缓存） |

use crate::cmds;
use crate::settings::Settings;
use crate::parse::{self, Iface, KEvent, RadioRaw, Station, StaDetail, Sys, VapRaw};
use crate::store::{now, Event, Store};
use crate::telnet::Session;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot, watch};


#[derive(Clone, Default, Serialize)]
pub struct CmdStat {
    pub cmd: String,
    pub every: String,
    pub count: u64,
    pub errors: u64,
    pub last_ms: f64,
    pub avg_ms: f64,
    pub last_bytes: usize,
    pub last_t: i64,
}

#[derive(Clone, Default, Serialize)]
pub struct RadioNow {
    pub ssid: String,
    pub mode: String,
    pub iface: Option<Iface>,
    pub temp: Option<i32>,
    pub therm_level: Option<i32>,
    pub nf: Option<i32>,
    pub busy: Option<f32>,
    pub tx: Option<f32>,
    pub rx: Option<f32>,
    pub self_util: Option<u32>,
    pub obss_util: Option<u32>,
    pub tx_mbps: Option<f32>,
    pub rx_mbps: Option<f32>,
    pub clients: usize,
}

#[derive(Clone, Serialize)]
pub struct ClientNow {
    #[serde(flatten)]
    pub st: Station,
    pub online: bool,
    pub first_seen: i64,
    pub last_seen: i64,
    pub snr: Option<i32>,
}

/// 页面需要的最新状态，采集任务写、HTTP 读。
#[derive(Default, Serialize)]
pub struct Live {
    pub connected: bool,
    pub connected_since: Option<i64>,
    pub last_error: Option<String>,
    pub last_error_t: Option<i64>,
    pub reconnects: u64,
    pub last_poll: Option<i64>,
    pub ap_uptime: Option<f64>,
    pub ap_boot: Option<i64>,
    pub ap_cpu: Option<f32>,
    pub ap_mem_total: Option<u64>,
    pub ap_mem_avail: Option<u64>,
    pub ap_load: Option<f32>,
    pub radios: [RadioNow; 2],
    pub clients: BTreeMap<String, ClientNow>,
    pub cmds: BTreeMap<String, CmdStat>,
    pub started: i64,
    /// 采集已暂停的时刻
    pub paused_since: Option<i64>,
}

pub type Shared<T> = Arc<RwLock<T>>;

pub enum Req {
    Sta(String, oneshot::Sender<Result<StaDetail, String>>),
}

enum Exit {
    Reload,
    Err(anyhow::Error),
}

/// 各档周期的显示文字（采集页的“周期”列）
struct Labels {
    fast: String,
    dmesg: String,
    slow: String,
}

fn secs_label(s: u64) -> String {
    if s.is_multiple_of(60) { format!("{} 分钟", s / 60) } else { format!("{s} 秒") }
}

struct Prev {
    sys: Option<Sys>,
    radio: [Option<RadioRaw>; 2],
    vap: [Option<VapRaw>; 2],
    sys_t: Instant,
    radio_t: [Option<Instant>; 2],
    iface: [Option<Iface>; 2],
    seen: Option<HashMap<String, u8>>,
    boot: Option<i64>,
    temp: [Option<(i32, i32)>; 2],
}

impl Prev {
    fn new() -> Prev {
        Prev {
            sys: None,
            radio: [None, None],
            vap: [None, None],
            sys_t: Instant::now(),
            radio_t: [None, None],
            iface: [None, None],
            seen: None,
            boot: None,
            temp: [None, None],
        }
    }
}

pub async fn run(settings: Shared<Settings>, store: Shared<Store>, live: Shared<Live>, mut rx: mpsc::Receiver<Req>, mut reload: watch::Receiver<()>) {
    let mut backoff = 60u64;
    let mut prev = Prev::new();
    let mut wait_first = false;
    let mut sta_cache: HashMap<String, (Instant, StaDetail)> = HashMap::new();
    loop {
        if wait_first {
            // 连不上时，按需请求直接回错误，不排队；改了设置则立即重试
            let wait = tokio::time::sleep(Duration::from_secs(backoff));
            tokio::pin!(wait);
            let mut reloaded = false;
            loop {
                tokio::select! {
                    _ = &mut wait => break,
                    _ = reload.changed() => { reloaded = true; break; }
                    Some(req) = rx.recv() => match req {
                        Req::Sta(_, tx) => { let _ = tx.send(Err("AP 未连接".into())); }
                    }
                }
            }
            backoff = if reloaded { 60 } else { (backoff * 2).min(600) };
        }
        wait_first = true;
        reload.borrow_and_update();
        let cfg = settings.read().unwrap().clone();
        if let Some(since) = cfg.paused_since {
            {
                let mut l = live.write().unwrap();
                l.connected = false;
                l.paused_since = Some(since);
                l.last_error = None;
            }
            // 恢复后不拿暂停前的计数算差值，也不把暂停期间的上下线补报成事件
            prev = Prev::new();
            loop {
                tokio::select! {
                    _ = reload.changed() => break,
                    req = rx.recv() => match req {
                        Some(Req::Sta(_, tx)) => { let _ = tx.send(Err("采集已暂停".into())); }
                        None => return,
                    }
                }
            }
            wait_first = false;
            continue;
        }
        live.write().unwrap().paused_since = None;
        if cfg.password.is_empty() || cfg.unlock.is_empty() {
            fail(&live, &store, "还没有设置 AP 密码或解锁串，请在设置页填写".into());
            continue;
        }
        let addr = format!("{}:23", cfg.ap_host);
        let session = tokio::select! {
            biased;
            _ = reload.changed() => {
                wait_first = false;
                continue;
            }
            result = Session::connect(&addr, &cfg.password, &cfg.unlock) => result,
        };
        let mut sess = match session {
            Ok(s) => s,
            Err(e) => {
                fail(&live, &store, format!("登录失败：{e:#}"));
                continue;
            }
        };
        let reconnecting = {
            let mut l = live.write().unwrap();
            l.connected = true;
            l.connected_since = Some(now());
            l.last_error = None;
            l.reconnects += 1;
            l.reconnects > 1
        };
        if reconnecting {
            store.write().unwrap().event(ev("collector", None, None, "采集已重新连接 AP".into()));
        }
        let exit = tokio::select! {
            biased;
            _ = reload.changed() => Exit::Reload,
            exit = session_loop(&mut sess, &cfg, &store, &live, &mut prev, &mut rx, &mut sta_cache, &mut backoff) => exit,
        };
        match exit {
            Exit::Reload => {
                live.write().unwrap().connected = false;
                // 暂停由接口记事件，这里只记设置变化
                if settings.read().unwrap().paused_since.is_none() {
                    store.write().unwrap().event(ev("collector", None, None, "设置已更新，按新设置重新连接".into()));
                }
                wait_first = false;
            }
            Exit::Err(err) => fail(&live, &store, format!("会话中断：{err:#}")),
        }
    }
}

fn fail(live: &Shared<Live>, store: &Shared<Store>, msg: String) {
    tracing::warn!("{msg}");
    let mut l = live.write().unwrap();
    let was = l.connected;
    l.connected = false;
    l.last_error = Some(msg.clone());
    l.last_error_t = Some(now());
    drop(l);
    if was {
        store.write().unwrap().event(ev("collector", None, None, msg));
    }
}

fn ev(kind: &str, band: Option<u8>, mac: Option<String>, msg: String) -> Event {
    Event { t: now(), kind: kind.into(), band, mac, msg, kts: None }
}

async fn exec(sess: &mut Session, live: &Shared<Live>, key: &str, cmd: &str, every: &str, secs: u64) -> anyhow::Result<String> {
    let r = sess.run(cmd, secs).await;
    let mut l = live.write().unwrap();
    let st = l.cmds.entry(key.to_string()).or_insert_with(|| CmdStat { cmd: cmd.to_string(), ..Default::default() });
    st.every = every.to_string();
    match &r {
        Ok(o) => {
            let ms = o.elapsed.as_secs_f64() * 1000.0;
            st.count += 1;
            st.last_ms = ms;
            st.avg_ms = if st.count == 1 { ms } else { st.avg_ms * 0.9 + ms * 0.1 };
            st.last_bytes = o.bytes;
            st.last_t = now();
        }
        Err(_) => st.errors += 1,
    }
    r.map(|o| o.text)
}

#[allow(clippy::too_many_arguments)]
async fn session_loop(
    sess: &mut Session,
    cfg: &Settings,
    store: &Shared<Store>,
    live: &Shared<Live>,
    prev: &mut Prev,
    rx: &mut mpsc::Receiver<Req>,
    sta_cache: &mut HashMap<String, (Instant, StaDetail)>,
    backoff: &mut u64,
) -> Exit {
    let lab = Labels { fast: secs_label(cfg.fast_secs), dmesg: secs_label(cfg.dmesg_secs), slow: secs_label(cfg.slow_secs) };
    let mut tick = tokio::time::interval(Duration::from_secs(cfg.fast_secs));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let due = |last: Option<Instant>, every: u64| last.is_none_or(|t| t.elapsed() + Duration::from_secs(2) >= Duration::from_secs(every));
    let (mut last_slow, mut last_dmesg): (Option<Instant>, Option<Instant>) = (None, None);
    let mut need_full_dmesg = true;
    loop {
        tokio::select! {
            _ = tick.tick() => {
                match fast(sess, store, live, prev, &lab).await {
                    Ok(rebooted) => need_full_dmesg |= rebooted,
                    Err(e) => return Exit::Err(e),
                }
                *backoff = 60;
                if due(last_slow, cfg.slow_secs) {
                    last_slow = Some(Instant::now());
                    if let Err(e) = slow(sess, store, live, prev, &lab).await { return Exit::Err(e); }
                    names(cfg, store);
                }
                if due(last_dmesg, cfg.dmesg_secs) || need_full_dmesg {
                    last_dmesg = Some(Instant::now());
                    match kernel(sess, store, live, prev, need_full_dmesg, &lab).await {
                        Ok(true) if !need_full_dmesg && prev.boot.is_some() => {
                            if let Err(e) = kernel(sess, store, live, prev, true, &lab).await {
                                return Exit::Err(e);
                            }
                            need_full_dmesg = false;
                        }
                        Ok(wait_for_boot) => need_full_dmesg = wait_for_boot,
                        Err(e) => return Exit::Err(e),
                    }
                }
            }
            Some(req) = rx.recv() => match req {
                Req::Sta(mac, tx) => {
                    if tx.is_closed() { continue; }
                    if let Some((t, d)) = sta_cache.get(&mac) {
                        if t.elapsed() < Duration::from_secs(10) { let _ = tx.send(Ok(d.clone())); continue; }
                    }
                    let Some(cmd) = cmds::sta(&mac) else { let _ = tx.send(Err("MAC 格式不对".into())); continue; };
                    match exec(sess, live, "sta", &cmd, "按需", 15).await {
                        Ok(text) => {
                            let d = parse::sta_detail(&text);
                            sta_cache.retain(|_, (t, _)| t.elapsed() < Duration::from_secs(60));
                            sta_cache.insert(mac, (Instant::now(), d.clone()));
                            let _ = tx.send(Ok(d));
                        }
                        Err(e) => { let _ = tx.send(Err(format!("{e:#}"))); return Exit::Err(e); }
                    }
                }
            }
        }
    }
}

fn f(x: Option<f32>) -> f32 {
    x.unwrap_or(f32::NAN)
}

struct Sample<'a>(&'a Shared<Store>);

impl<'a> Sample<'a> {
    fn start(store: &'a Shared<Store>, t: i64) -> Self {
        store.write().unwrap().sample_started = Some(t);
        Self(store)
    }
}

impl Drop for Sample<'_> {
    fn drop(&mut self) {
        self.0.write().unwrap().sample_started = None;
    }
}

#[allow(clippy::needless_range_loop)]
async fn fast(sess: &mut Session, store: &Shared<Store>, live: &Shared<Live>, prev: &mut Prev, lab: &Labels) -> anyhow::Result<bool> {
    let t = now();
    let _sample = Sample::start(store, t);
    let sys_text = exec(sess, live, "sys", cmds::SYS, &lab.fast, 15).await?;
    let sys_at = Instant::now();
    let iw_text = exec(sess, live, "iw", cmds::IW, &lab.fast, 15).await?;
    let mut stas: Vec<Station> = Vec::new();
    for b in 0..2 {
        let text = exec(sess, live, &format!("wlan{b}"), cmds::WLAN[b], &lab.fast, 20).await?;
        stas.extend(parse::stations(&text, b as u8));
    }
    let mut radios: [Option<RadioRaw>; 2] = [None, None];
    let mut radio_at: [Option<Instant>; 2] = [None, None];
    for b in 0..2 {
        let text = exec(sess, live, &format!("radio{b}"), cmds::RADIO[b], &lab.fast, 20).await?;
        radios[b] = parse::radio(&text);
        radio_at[b] = Some(Instant::now());
    }

    let sys = parse::sys(&sys_text);
    let ifs = parse::iw(&iw_text);
    let mut st = store.write().unwrap();
    let mut l = live.write().unwrap();
    l.last_poll = Some(t);

    // AP 系统状态与重启检测
    let mut if_rate: [(Option<f32>, Option<f32>); 2] = [(None, None); 2];
    let mut rebooted = false;
    if let Some(s) = &sys {
        let boot = s.date - s.uptime as i64;
        rebooted = prev.sys.as_ref().is_some_and(|p| ap_rebooted(p.uptime, s.uptime, sys_at.duration_since(prev.sys_t)));
        if rebooted {
            let since = crate::store::wall_time(l.ap_boot.unwrap_or(boot));
            st.event(ev("ap_reboot", None, None, format!("AP 已重启（上次开机 {since}）")));
            st.reset_kts();
            prev.seen = None;
            prev.sys = None;
            prev.radio = [None, None];
            prev.radio_t = [None, None];
            prev.vap = [None, None];
        }
        // 用 uptime 判断重启，系统校时不能把同一次开机的日志再计一遍。
        prev.boot = Some(if rebooted { boot } else { prev.boot.unwrap_or(boot) });
        l.ap_boot = Some(boot);
        l.ap_uptime = Some(s.uptime);
        l.ap_mem_total = Some(s.mem_total);
        l.ap_mem_avail = Some(s.mem_avail);
        l.ap_load = Some(s.load1);
        let mut cpu = None;
        if let Some(p) = &prev.sys {
            let (db, dt) = (s.cpu.0.saturating_sub(p.cpu.0), s.cpu.1.saturating_sub(p.cpu.1));
            if dt > 0 && s.cpu.1 > p.cpu.1 {
                cpu = Some((db as f64 / dt as f64 * 100.0) as f32);
            }
            let secs = sys_at.duration_since(prev.sys_t).as_secs_f64();
            if secs > 1.0 {
                for b in 0..2 {
                    let (rx0, tx0) = p.ifbytes[b];
                    let (rx1, tx1) = s.ifbytes[b];
                    if rx1 >= rx0 && tx1 >= tx0 {
                        // 以 AP 视角：tx 是发给终端（下行），rx 是终端上行
                        if_rate[b] = (
                            Some(((tx1 - tx0) as f64 * 8.0 / secs / 1e6) as f32),
                            Some(((rx1 - rx0) as f64 * 8.0 / secs / 1e6) as f32),
                        );
                    }
                }
            }
        }
        l.ap_cpu = cpu;
        let mem_pct = if s.mem_total > 0 { (1.0 - s.mem_avail as f64 / s.mem_total as f64) as f32 * 100.0 } else { f32::NAN };
        st.push("ap", t, &[f(cpu), mem_pct, s.load1], t);
        prev.sys = Some(s.clone());
        prev.sys_t = sys_at;
    }

    // 信道/频宽变化
    for b in 0..2 {
        if let (Some(old), Some(new)) = (&prev.iface[b], &ifs[b]) {
            if old.chan != new.chan || old.width != new.width {
                let kind = if old.width != new.width { "width" } else { "chan" };
                st.event(ev(
                    kind,
                    Some(b as u8),
                    None,
                    format!("信道 {}/{}MHz → {}/{}MHz", old.chan, old.width, new.chan, new.width),
                ));
            }
        }
        if ifs[b].is_some() {
            prev.iface[b] = ifs[b].clone();
            l.radios[b].iface = ifs[b].clone();
        }
    }

    // 客户端：上线、离线、换频段
    let now_seen: HashMap<String, u8> = stas.iter().map(|s| (s.mac.clone(), s.band)).collect();
    if let Some(old) = &prev.seen {
        for (mac, b) in &now_seen {
            match old.get(mac) {
                None => st.event(ev("online", Some(*b), Some(mac.clone()), "上线".into())),
                Some(ob) if ob != b => st.event(ev(
                    "roam",
                    Some(*b),
                    Some(mac.clone()),
                    format!("切换频段 {} → {}", band_name(*ob), band_name(*b)),
                )),
                _ => {}
            }
        }
        for (mac, b) in old {
            if !now_seen.contains_key(mac) {
                let assoc = l.clients.get(mac).map(|c| c.st.assoc_secs).unwrap_or(0);
                st.event(ev("offline", Some(*b), Some(mac.clone()), format!("离线（本次连接 {}）", dur(assoc))));
            }
        }
    }
    for c in l.clients.values_mut() {
        c.online = now_seen.contains_key(&c.st.mac);
    }
    prev.seen = Some(now_seen);
    let mut counts = [0usize; 2];
    for s in stas {
        counts[s.band as usize] += 1;
        let nf = prev.radio[s.band as usize].map(|r| r.nf).or(radios[s.band as usize].map(|r| r.nf));
        let snr = nf.map(|n| s.rssi - n);
        st.push(
            &format!("c:{}", s.mac),
            t,
            &[s.rssi as f32, s.rssi_min as f32, s.tx_rate, s.rx_rate, s.idle as f32, s.band as f32],
            t,
        );
        st.touch_seen(&s.mac, s.band, t);
        match l.clients.entry(s.mac.clone()) {
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                let c = entry.get_mut();
                c.st = s;
                c.online = true;
                c.last_seen = t;
                c.snr = snr;
            }
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(ClientNow { st: s, online: true, first_seen: t, last_seen: t, snr });
            }
        }
    }
    // 24 小时没见过的客户端从实时列表移除，历史数据仍在 store
    l.clients.retain(|_, c| c.online || c.last_seen > t - 86400);

    // 射频：繁忙率来自两次计数之差
    for b in 0..2 {
        let Some(r) = radios[b] else { continue };
        let mut busy = None;
        let mut phy_ps = None;
        let mut tx_fail = None;
        if let (Some(p), Some(pt), Some(nt)) = (prev.radio[b], prev.radio_t[b], radio_at[b]) {
            let elapsed = nt.duration_since(pt);
            busy = radio_busy(&p, &r, elapsed);
            let secs = elapsed.as_secs_f64();
            if secs > 1.0 && r.phy_err >= p.phy_err {
                phy_ps = Some(((r.phy_err - p.phy_err) as f64 / secs) as f32);
            }
            if r.tx_fail >= p.tx_fail {
                tx_fail = Some((r.tx_fail - p.tx_fail) as f32);
            }
        }
        let rn = &mut l.radios[b];
        rn.nf = Some(r.nf);
        rn.self_util = r.self_util;
        rn.obss_util = r.obss_util;
        rn.clients = counts[b];
        rn.tx_mbps = if_rate[b].0;
        rn.rx_mbps = if_rate[b].1;
        rn.busy = busy.map(|x| x.busy);
        rn.tx = busy.map(|x| x.tx);
        rn.rx = busy.map(|x| x.rx);
        let other = busy.map(|x| (x.busy - x.tx - x.rx).max(0.0));
        let ifc = &ifs[b];
        st.push(
            &format!("radio{b}"),
            t,
            &[
                f(busy.map(|x| x.busy)),
                f(busy.map(|x| x.tx)),
                f(busy.map(|x| x.rx)),
                f(other),
                r.self_util.map(|x| x as f32).unwrap_or(f32::NAN),
                r.obss_util.map(|x| x as f32).unwrap_or(f32::NAN),
                r.nf as f32,
                counts[b] as f32,
                f(if_rate[b].0),
                f(if_rate[b].1),
                f(phy_ps),
                f(tx_fail),
                ifc.as_ref().map(|i| i.width as f32).unwrap_or(f32::NAN),
                ifc.as_ref().map(|i| i.chan as f32).unwrap_or(f32::NAN),
                f(busy.map(|x| x.busy)),
            ],
            t,
        );
        prev.radio[b] = Some(r);
        prev.radio_t[b] = radio_at[b];
    }
    Ok(rebooted)
}

fn ap_rebooted(previous_uptime: f64, uptime: f64, elapsed: Duration) -> bool {
    // 允许采集和两端时钟有几秒误差，仍能识别断线期间重启、uptime 已涨回上次读数的情况。
    uptime + 1.0 < previous_uptime || uptime + 30.0 < previous_uptime + elapsed.as_secs_f64()
}

fn radio_busy(previous: &RadioRaw, current: &RadioRaw, elapsed: Duration) -> Option<parse::Busy> {
    // 计数约每微秒加一，超过一个 32 位周期时无法由端点唯一还原增量。
    (elapsed.as_micros() < u32::MAX as u128).then(|| parse::busy(previous, current)).flatten()
}

async fn slow(sess: &mut Session, store: &Shared<Store>, live: &Shared<Live>, prev: &mut Prev, lab: &Labels) -> anyhow::Result<()> {
    let t = now();
    let _sample = Sample::start(store, t);
    let mut vaps = [None, None];
    let mut temps = [None, None];
    for b in 0..2 {
        vaps[b] = parse::vap(&exec(sess, live, &format!("vap{b}"), cmds::VAP[b], &lab.slow, 20).await?);
        temps[b] = parse::therm(&exec(sess, live, &format!("therm{b}"), cmds::THERM[b], &lab.slow, 20).await?);
    }
    let ss = parse::ssid(&exec(sess, live, "ssid", cmds::SSID, &lab.slow, 15).await?);
    let mut st = store.write().unwrap();
    let mut l = live.write().unwrap();
    for b in 0..2 {
        if !ss[b].0.is_empty() {
            l.radios[b].ssid = ss[b].0.clone();
        }
        if !ss[b].1.is_empty() {
            l.radios[b].mode = ss[b].1.clone();
        }
        if let Some((tc, lv)) = temps[b] {
            l.radios[b].temp = Some(tc);
            l.radios[b].therm_level = Some(lv);
            if lv > 0 && prev.temp[b].map(|x| x.1) == Some(0) {
                st.event(ev("thermal", Some(b as u8), None, format!("{} 进入降频档位 {lv}（{tc}°C）", band_name(b as u8))));
            }
            prev.temp[b] = Some((tc, lv));
        }
        let Some(v) = vaps[b] else { continue };
        let mut row = [f32::NAN; 10];
        if let Some(p) = prev.vap[b] {
            let d = |a: u64, b: u64| if b >= a { Some((b - a) as f32) } else { None };
            let dtx = d(p.tx_pkts, v.tx_pkts);
            let dre = d(p.retries, v.retries);
            row[0] = match (dtx, dre) {
                (Some(x), Some(r)) if x + r > 0.0 => r / (x + r) * 100.0,
                _ => f32::NAN,
            };
            row[1] = f(d(p.auth_att, v.auth_att));
            row[2] = f(d(p.auth_ok, v.auth_ok));
            row[3] = f(d(p.beacon_fail, v.beacon_fail));
            row[4] = f(d(p.probe_drop, v.probe_drop));
            row[5] = f(d(p.tx_fail, v.tx_fail));
        }
        row[6] = temps[b].map(|x| x.0 as f32).unwrap_or(f32::NAN);
        row[7] = temps[b].map(|x| x.1 as f32).unwrap_or(f32::NAN);
        if let Some(p) = prev.vap[b] {
            if v.tx_pkts >= p.tx_pkts && v.retries >= p.retries {
                row[8] = (v.tx_pkts - p.tx_pkts) as f32;
                row[9] = (v.retries - p.retries) as f32;
            }
        }
        st.push(&format!("vap{b}"), t, &row, t);
        prev.vap[b] = Some(v);
    }
    Ok(())
}

/// 读内核日志。返回 true 表示需要全量，或尚无开机时间以计算日志时刻。
async fn kernel(sess: &mut Session, store: &Shared<Store>, live: &Shared<Live>, prev: &Prev, full: bool, lab: &Labels) -> anyhow::Result<bool> {
    let Some(boot) = prev.boot else { return Ok(true) };
    let (key, cmd, every) = if full { ("dmesg_full", cmds::DMESG_FULL, "连上时一次") } else { ("dmesg", cmds::DMESG_TAIL, lab.dmesg.as_str()) };
    let text = exec(sess, live, key, cmd, every, 30).await?;
    let wall_boot = prev.sys.as_ref().map_or(boot, |s| s.date - s.uptime as i64);
    Ok(record_kernel(&mut store.write().unwrap(), boot, wall_boot, full, &text))
}

fn record_kernel(st: &mut Store, boot: i64, wall_boot: i64, full: bool, text: &str) -> bool {
    let kmax = st.max_kts(boot);
    let mut lines = parse::dmesg(text);
    let gap = !full && kmax > 0.0 && text.lines().count() >= 40 && !lines.is_empty()
        && lines.iter().all(|(ts, _)| *ts > kmax);
    if gap {
        // 保留缺口之前的游标。全量补拉会一次解析缺口和这批尾部，避免重复。
        st.event(ev("gap", None, None, "内核日志尾部全是新内容，补拉全量".into()));
        return true;
    }
    lines.retain(|(ts, _)| *ts > kmax);
    for (ts, k) in parse::kevents(&lines) {
        let t = wall_boot + ts as i64;
        let (kind, band, mac, msg) = match k {
            KEvent::Assoc { mac, band } => ("assoc", band, Some(mac), "完成关联握手".to_string()),
            KEvent::Radar { freq } => ("radar", Some(1), None, format!("检测到雷达（{freq} MHz），按规定避让")),
            KEvent::DfsSwitch { width, chan } => ("dfs", Some(1), None, format!("DFS 避让：切到信道 {chan}，{width}MHz")),
            KEvent::PreCacDone { freq } => ("precac", Some(1), None, format!("后台雷达检测（PreCAC）完成：{freq} MHz")),
            KEvent::RadioRestart { band, clients } => ("radio_restart", band, None, format!("射频重启，断开 {clients} 个终端")),
        };
        st.event(Event { t, kind: kind.into(), band, mac, msg, kts: Some(ts) });
    }
    // 推进游标：即使本批没有可识别事件也要记下，避免重复处理
    if let Some(mx) = lines.iter().map(|x| x.0).reduce(f64::max) {
        st.set_kts(boot, mx);
    }
    false
}

fn names(cfg: &Settings, store: &Shared<Store>) {
    if let Ok(text) = std::fs::read_to_string("/proc/net/arp") {
        store.write().unwrap().set_arp(crate::names::parse_arp(&text));
    }
    if !cfg.leases_path.is_empty() {
        if let Ok(text) = std::fs::read_to_string(&cfg.leases_path) {
            let mut st = store.write().unwrap();
            for (mac, n) in crate::names::parse_leases(&text) {
                st.set_dhcp(&mac, n);
            }
        }
    }
}

pub fn band_name(b: u8) -> &'static str {
    if b == 0 {
        "2.4G"
    } else {
        "5G"
    }
}

pub fn dur(s: u64) -> String {
    if s >= 86400 {
        format!("{} 天 {} 小时", s / 86400, s % 86400 / 3600)
    } else if s >= 3600 {
        format!("{} 小时 {} 分", s / 3600, s % 3600 / 60)
    } else {
        format!("{} 分 {} 秒", s / 60, s % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_dmesg_fills_gap_without_duplicates() {
        let boot = now() - 100;
        let mut st = Store::new(None);
        st.set_kts(boot, 10.0);
        let tail = (12..52).map(|t| format!("[{t}] tail\n")).collect::<String>();
        assert!(record_kernel(&mut st, boot, boot, false, &tail));
        assert_eq!(st.max_kts(boot), 10.0);

        let full = format!("[9] Radar found\n[11] Radarfound Freq=5500\n{tail}[52] Radar found\n");
        assert!(!record_kernel(&mut st, boot, boot, true, &full));
        assert_eq!(st.max_kts(boot), 52.0);
        assert!(!record_kernel(&mut st, boot, boot, true, &full));
        let radar: Vec<_> = st.events.iter().filter(|e| e.kind == "radar").map(|e| e.kts).collect();
        assert_eq!(radar, [Some(11.0), Some(52.0)]);
    }

    #[test]
    fn reboot_and_long_counter_gap_are_distinguished_from_normal_wrap() {
        assert!(ap_rebooted(400.0, 12.0, Duration::from_secs(30)));
        assert!(ap_rebooted(400.0, 410.0, Duration::from_secs(300)));
        assert!(!ap_rebooted(400.0, 702.0, Duration::from_secs(300)));
        let prev = RadioRaw { cycle: u32::MAX - 50, ..Default::default() };
        let next = RadioRaw { cycle: 30_000_000, rx_clear: 12_000_000, ..Default::default() };
        assert!(radio_busy(&prev, &next, Duration::from_secs(30)).is_some());
        assert!(radio_busy(&prev, &next, Duration::from_secs(4300)).is_none());
    }
}
