//! HTTP 接口与健康发现。
//!
//! 观测接口只读；/api/sta 会按需在 AP 上执行一次 apstats（有缓存）。
//! 改动类接口（/api/login 之外的 POST）要求已登录且带 `X-Apw: 1`；控制操作经官方 Web 接口执行。

use crate::auth::{self, Auth, Login};
use crate::collector::{band_name, dur, Live, Req, Shared};
use crate::ctl::{Ctl, Op};
use crate::settings::Settings;
use crate::store::{now, schema, Event, Store};
use axum::extract::{ConnectInfo, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use tokio::sync::{mpsc, oneshot};

#[derive(Clone)]
pub struct App {
    pub store: Shared<Store>,
    pub live: Shared<Live>,
    pub tx: mpsc::Sender<Req>,
    pub settings: Shared<Settings>,
    pub ctl: Arc<Ctl>,
    pub auth: Arc<Auth>,
    pub data_dir: Option<PathBuf>,
}

pub fn router(app: App) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/overview", get(overview))
        .route("/api/clients", get(clients))
        .route("/api/devices", get(devices))
        .route("/api/client", get(client))
        .route("/api/sta", get(sta))
        .route("/api/series", get(series))
        .route("/api/events", get(events))
        .route("/api/collector", get(collector))
        .route("/api/collector/pause", post(collector_pause))
        .route("/api/session", get(session))
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route("/api/settings", get(settings_get).post(settings_set))
        .route("/api/alias", post(alias))
        .route("/api/ctl", get(ctl_state))
        .route("/api/ctl/radio", post(ctl_radio))
        .route("/api/ctl/block", post(ctl_block))
        .route("/api/ctl/cancel", post(ctl_cancel))
        .with_state(app)
}

const INDEX: &str = include_str!("../assets/index.html");

async fn index() -> Response {
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8"), (header::CACHE_CONTROL, "no-cache")], INDEX).into_response()
}

pub fn range_secs(r: Option<&str>) -> i64 {
    match r.unwrap_or("24h") {
        "1h" => 3600,
        "6h" => 6 * 3600,
        "7d" => 7 * 86400,
        "30d" => 30 * 86400,
        _ => 86400,
    }
}

fn columns(key: &str, rows: &[(i64, Vec<f32>)]) -> Value {
    let sc = schema(key);
    let mut cols = serde_json::Map::new();
    for (i, n) in sc.names.iter().enumerate() {
        let v: Vec<Value> = rows
            .iter()
            .map(|(_, r)| r.get(i).filter(|x| x.is_finite()).map(|x| json!((*x * 100.0).round() / 100.0)).unwrap_or(Value::Null))
            .collect();
        cols.insert(n.to_string(), Value::Array(v));
    }
    json!({ "t": rows.iter().map(|r| r.0).collect::<Vec<_>>(), "v": cols })
}

#[derive(Deserialize)]
struct SeriesQ {
    key: String,
    range: Option<String>,
    points: Option<usize>,
}

async fn series(State(app): State<App>, Query(q): Query<SeriesQ>) -> Json<Value> {
    let t = now();
    let from = t - range_secs(q.range.as_deref());
    let pts = q.points.unwrap_or(720).clamp(10, 3000);
    let st = app.store.read().unwrap();
    let rows = st
        .series
        .get(&q.key)
        .map(|s| s.query(from, t, pts, schema(&q.key).aggs))
        .unwrap_or_default();
    Json(json!({ "key": q.key, "from": from, "to": t, "data": columns(&q.key, &rows) }))
}

#[derive(Deserialize)]
struct EventsQ {
    kind: Option<String>,
    mac: Option<String>,
    band: Option<u8>,
    limit: Option<usize>,
    range: Option<String>,
}

async fn events(State(app): State<App>, Query(q): Query<EventsQ>) -> Json<Value> {
    let from = now() - range_secs(q.range.as_deref().or(Some("7d")));
    let kinds: Option<Vec<&str>> = q.kind.as_deref().filter(|k| !k.is_empty()).map(|k| k.split(',').collect());
    let st = app.store.read().unwrap();
    let mut counts: HashMap<String, usize> = HashMap::new();
    let list: Vec<&Event> = st
        .events
        .iter()
        .rev()
        .filter(|e| e.t >= from)
        .filter(|e| q.mac.as_deref().is_none_or(|m| e.mac.as_deref() == Some(m)))
        .filter(|e| q.band.is_none_or(|b| e.band == Some(b)))
        .inspect(|e| *counts.entry(e.kind.clone()).or_default() += 1)
        .filter(|e| kinds.as_ref().is_none_or(|k| k.contains(&e.kind.as_str())))
        .collect();
    let limit = q.limit.unwrap_or(500).min(5000);
    let total = list.len();
    let out: Vec<Value> = list.into_iter().take(limit).map(|e| event_json(e, &st)).collect();
    Json(json!({ "events": out, "total": total, "counts": counts }))
}

fn event_json(e: &Event, st: &Store) -> Value {
    let name = e.mac.as_ref().and_then(|m| st.resolve(m).name);
    json!({ "t": e.t, "kind": e.kind, "band": e.band, "mac": e.mac, "name": name, "msg": e.msg })
}

fn median(mut v: Vec<f32>) -> Option<f32> {
    v.retain(|x| x.is_finite());
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    Some(v[v.len() / 2])
}

fn recent(st: &Store, key: &str, field: usize, secs: i64, t: i64) -> Vec<f32> {
    st.recent(key, field, secs, t)
}

fn assoc_counts(st: &Store, since: i64) -> HashMap<String, usize> {
    let mut m = HashMap::new();
    for e in st.events.iter().rev().take_while(|e| e.t >= since) {
        if e.kind == "assoc" {
            if let Some(mac) = &e.mac {
                *m.entry(mac.clone()).or_default() += 1;
            }
        }
    }
    m
}

#[derive(Serialize)]
struct Finding {
    level: &'static str,
    title: String,
    detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    mac: Option<String>,
}

fn display_name(st: &Store, mac: &str) -> String {
    let r = st.resolve(mac);
    r.name.or(r.ip).unwrap_or_else(|| mac.to_string())
}

/// 雷达事件及其持续时间：雷达 → 下一次 PreCAC 完成（恢复原信道）视为恢复。
fn radar_spans(st: &Store, since: i64) -> Vec<(i64, Option<i64>, String)> {
    let ev: Vec<&Event> = st.events.iter().filter(|e| e.t >= since).collect();
    let mut out = Vec::new();
    for (i, e) in ev.iter().enumerate() {
        if e.kind == "radar" {
            let end = ev[i + 1..]
                .iter()
                .find(|x| (x.kind == "precac" || x.kind == "width") && x.t > e.t && x.t - e.t < 4 * 3600)
                .map(|x| x.t);
            out.push((e.t, end, e.msg.clone()));
        }
    }
    out
}

fn findings(st: &Store, live: &Live, t: i64) -> (Vec<Finding>, u32) {
    let mut f = Vec::new();
    let mut score: i32 = 100;
    // 信号偏弱
    let mut weak = 0;
    for c in live.clients.values().filter(|c| c.online) {
        let med = median(recent(st, &format!("c:{}", c.st.mac), 0, 3600, t));
        let thr = if c.st.band == 1 { -72.0 } else { -75.0 };
        if let Some(m) = med.filter(|m| *m < thr) {
            weak += 1;
            f.push(Finding {
                level: "warn",
                title: format!("{} 信号偏弱", display_name(st, &c.st.mac)),
                detail: format!(
                    "{} 近 1 小时信号中位数 {:.0} dBm（低于 {thr} dBm），当前速率 发 {:.0} / 收 {:.0} Mbps",
                    band_name(c.st.band),
                    m,
                    c.st.tx_rate,
                    c.st.rx_rate
                ),
                mac: Some(c.st.mac.clone()),
            });
        }
    }
    score -= (weak * 4).min(20);
    // 频繁重连
    let mut rc = 0;
    let mut ac: Vec<(String, usize)> = assoc_counts(st, t - 86400).into_iter().filter(|(_, n)| *n > 12).collect();
    ac.sort_by_key(|x| std::cmp::Reverse(x.1));
    for (mac, n) in ac {
        rc += 1;
        f.push(Finding {
            level: "warn",
            title: format!("{} 频繁重连", display_name(st, &mac)),
            detail: format!("24 小时内关联握手 {n} 次。常见原因：信号边缘、设备省电策略、或密钥更新"),
            mac: Some(mac),
        });
    }
    score -= (rc * 3).min(15);
    // 信道拥堵
    for b in 0..2u8 {
        if let Some(m) = median(recent(st, &format!("radio{b}"), 0, 3600, t)) {
            let thr = if b == 0 { 50.0 } else { 60.0 };
            if m > thr {
                score -= 10;
                let obss = median(recent(st, &format!("radio{b}"), 5, 3600, t));
                f.push(Finding {
                    level: "bad",
                    title: format!("{} 信道繁忙", band_name(b)),
                    detail: format!(
                        "近 1 小时信道繁忙率中位数 {m:.0}%{}",
                        obss.map(|o| format!("，其中邻居占用约 {o:.0}%")).unwrap_or_default()
                    ),
                    mac: None,
                });
            }
        }
    }
    // 雷达
    let spans = radar_spans(st, t - 7 * 86400);
    if !spans.is_empty() {
        score -= 5;
        let total: i64 = spans.iter().filter_map(|(a, b, _)| b.map(|b| b - a)).sum();
        f.push(Finding {
            level: "info",
            title: format!("7 天内 5G 雷达避让 {} 次", spans.len()),
            detail: format!(
                "每次会断开 5G 终端并临时降到非 DFS 信道 / 80MHz，累计约 {}。这是保留 160MHz 的已知代价",
                dur(total.max(0) as u64)
            ),
            mac: None,
        });
    }
    // 温度
    for (b, r) in live.radios.iter().enumerate() {
        if let Some(tc) = r.temp.filter(|x| *x > 90) {
            score -= 15;
            f.push(Finding {
                level: "bad",
                title: format!("{} 射频温度偏高", band_name(b as u8)),
                detail: format!("{tc}°C，降频档位 {}", r.therm_level.unwrap_or(0)),
                mac: None,
            });
        }
    }
    // 重传：按包数加权；流量太小时比例会被少数包放大，不判定
    for b in 0..2u8 {
        let key = format!("vap{b}");
        let tx: f32 = recent(st, &key, 8, 3600, t).iter().filter(|x| x.is_finite()).sum();
        let re: f32 = recent(st, &key, 9, 3600, t).iter().filter(|x| x.is_finite()).sum();
        if tx + re >= 30_000.0 {
            let pct = re / (tx + re) * 100.0;
            if pct > 10.0 {
                score -= 10;
                f.push(Finding {
                    level: "warn",
                    title: format!("{} 重传偏高", band_name(b)),
                    detail: format!("近 1 小时重传占比 {pct:.1}%（{:.0} 万个包）", (tx + re) / 10_000.0),
                    mac: None,
                });
            }
        }
    }
    (f, score.clamp(0, 100) as u32)
}

async fn overview(State(app): State<App>) -> Json<Value> {
    let t = now();
    let st = app.store.read().unwrap();
    let live = app.live.read().unwrap();
    let (f, score) = findings(&st, &live, t);
    let important = ["radar", "dfs", "radio_restart", "width", "chan", "ap_reboot", "collector", "thermal", "roam", "gap", "control"];
    let ev: Vec<Value> = st
        .events
        .iter()
        .rev()
        .filter(|e| important.contains(&e.kind.as_str()) || e.kind == "online" || e.kind == "offline")
        .take(12)
        .map(|e| event_json(e, &st))
        .collect();
    let assoc24: usize = assoc_counts(&st, t - 86400).values().sum();
    let radar = radar_spans(&st, t - 7 * 86400)
        .into_iter()
        .map(|(a, b, m)| json!({ "t": a, "end": b, "msg": m }))
        .collect::<Vec<_>>();
    Json(json!({
        "now": t,
        "ap_host": app.settings.read().unwrap().ap_host,
        "connected": live.connected,
        "connected_since": live.connected_since,
        "paused_since": live.paused_since,
        "last_error": live.last_error,
        "last_poll": live.last_poll,
        "ap": {
            "host": "TL-XDR5480",
            "uptime": live.ap_uptime,
            "cpu": live.ap_cpu,
            "mem_total": live.ap_mem_total,
            "mem_avail": live.ap_mem_avail,
            "load": live.ap_load,
        },
        "radios": live.radios,
        "online": [
            live.clients.values().filter(|c| c.online && c.st.band == 0).count(),
            live.clients.values().filter(|c| c.online && c.st.band == 1).count(),
        ],
        "assoc24": assoc24,
        "findings": f,
        "score": score,
        "events": ev,
        "radar": radar,
    }))
}

async fn clients(State(app): State<App>) -> Json<Value> {
    let t = now();
    let st = app.store.read().unwrap();
    let live = app.live.read().unwrap();
    let ac = assoc_counts(&st, t - 86400);
    let list: Vec<Value> = live
        .clients
        .values()
        .map(|c| {
            let n = st.resolve(&c.st.mac);
            let key = format!("c:{}", c.st.mac);
            let spark: Vec<f32> = recent(&st, &key, 0, 3600, t);
            let step = (spark.len() / 40).max(1);
            json!({
                "mac": c.st.mac,
                "name": n.name,
                "name_src": n.name_src,
                "hostname": n.hostname,
                "ip": n.ip,
                "random_mac": is_random(&c.st.mac),
                "band": c.st.band,
                "chan": c.st.chan,
                "rssi": c.st.rssi,
                "rssi_min": c.st.rssi_min,
                "rssi_max": c.st.rssi_max,
                "rssi_med": median(spark.clone()),
                "snr": c.snr,
                "tx_rate": c.st.tx_rate,
                "rx_rate": c.st.rx_rate,
                "max_rate": c.st.max_rate,
                "mode": c.st.mode,
                "nss": c.st.nss,
                "ps": c.st.ps,
                "idle": c.st.idle,
                "assoc_secs": c.st.assoc_secs,
                "online": c.online,
                "first_seen": c.first_seen,
                "last_seen": c.last_seen,
                "assoc24": ac.get(&c.st.mac).copied().unwrap_or(0),
                "spark": spark.iter().step_by(step).map(|x| x.round() as i32).collect::<Vec<_>>(),
            })
        })
        .collect();
    Json(json!({ "clients": list, "now": t }))
}

/// 设备名册：连过 AP 的全部终端（不限 24 小时）和只登记了备注的设备。在线的排前面，其余按最近一次见到排序。
async fn devices(State(app): State<App>) -> Json<Value> {
    let st = app.store.read().unwrap();
    let live = app.live.read().unwrap();
    let mut list: Vec<(bool, i64, Value)> = st
        .known_macs()
        .into_iter()
        .map(|mac| {
            let n = st.resolve(&mac);
            let seen = st.seen.get(&mac);
            let lc = live.clients.get(&mac);
            let online = lc.is_some_and(|c| c.online);
            let last = seen.map(|x| x.last).unwrap_or(0);
            let v = json!({
                "mac": mac,
                "name": n.name,
                "name_src": n.name_src,
                "hostname": n.hostname,
                "alias": n.alias,
                "ip": n.ip,
                "random_mac": is_random(&mac),
                "online": online,
                "band": lc.map(|c| c.st.band).or(seen.map(|x| x.band)),
                "first_seen": seen.map(|x| x.first),
                "last_seen": seen.map(|x| x.last),
            });
            (online, last, v)
        })
        .collect();
    list.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
    Json(json!({ "devices": list.into_iter().map(|x| x.2).collect::<Vec<_>>(), "now": now() }))
}

pub fn is_random(mac: &str) -> bool {
    u8::from_str_radix(&mac[..2], 16).map(|b| b & 0x02 != 0).unwrap_or(false)
}

#[derive(Deserialize)]
struct ClientQ {
    mac: String,
    range: Option<String>,
}

async fn client(State(app): State<App>, Query(q): Query<ClientQ>) -> Response {
    let mac = q.mac.to_ascii_lowercase();
    if !crate::cmds::valid_mac(&mac) {
        return (StatusCode::BAD_REQUEST, "MAC 格式不对").into_response();
    }
    let t = now();
    let from = t - range_secs(q.range.as_deref());
    let st = app.store.read().unwrap();
    let live = app.live.read().unwrap();
    let key = format!("c:{mac}");
    let rows = st.series.get(&key).map(|s| s.query(from, t, 720, schema(&key).aggs)).unwrap_or_default();
    let ev: Vec<Value> = st
        .events
        .iter()
        .rev()
        .filter(|e| e.mac.as_deref() == Some(&mac) && e.t >= t - 7 * 86400)
        .take(300)
        .map(|e| event_json(e, &st))
        .collect();
    let n = st.resolve(&mac);
    Json(json!({
        "mac": mac,
        "name": n.name,
        "name_src": n.name_src,
        "hostname": n.hostname,
        "alias": n.alias,
        "ip": n.ip,
        "random_mac": is_random(&mac),
        "live": live.clients.get(&mac),
        "nf": live.clients.get(&mac).and_then(|c| live.radios[c.st.band as usize].nf),
        "series": columns(&key, &rows),
        "events": ev,
    }))
    .into_response()
}

#[derive(Deserialize)]
struct StaQ {
    mac: String,
}

async fn sta(State(app): State<App>, Query(q): Query<StaQ>) -> Response {
    let mac = q.mac.to_ascii_lowercase();
    if !crate::cmds::valid_mac(&mac) {
        return (StatusCode::BAD_REQUEST, "MAC 格式不对").into_response();
    }
    if !app.live.read().unwrap().clients.get(&mac).is_some_and(|c| c.online) {
        return (StatusCode::NOT_FOUND, "该设备当前不在线").into_response();
    }
    let (tx, rx) = oneshot::channel();
    if app.tx.try_send(Req::Sta(mac, tx)).is_err() {
        return (StatusCode::SERVICE_UNAVAILABLE, "采集忙，请稍后").into_response();
    }
    match tokio::time::timeout(std::time::Duration::from_secs(45), rx).await {
        Ok(Ok(Ok(d))) => Json(d).into_response(),
        Ok(Ok(Err(e))) => (StatusCode::BAD_GATEWAY, e).into_response(),
        _ => (StatusCode::GATEWAY_TIMEOUT, "等待 AP 超时").into_response(),
    }
}

async fn collector(State(app): State<App>) -> Json<Value> {
    let live = app.live.read().unwrap();
    let st = app.store.read().unwrap();
    let samples: usize = st.series.values().map(|s| s.rows.iter().map(|r| r.len()).sum::<usize>()).sum();
    Json(json!({
        "connected": live.connected,
        "connected_since": live.connected_since,
        "connects": live.reconnects,
        "last_error": live.last_error,
        "last_error_t": live.last_error_t,
        "last_poll": live.last_poll,
        "paused_since": live.paused_since,
        "started": live.started,
        "cmds": live.cmds,
        "whitelist": crate::cmds::all_fixed(),
        "store": { "series": st.series.len(), "samples": samples, "events": st.events.len() },
        "ap_cpu": live.ap_cpu,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vap_row(tx: f32, re: f32) -> Vec<f32> {
        let pct = re / (tx + re) * 100.0;
        vec![pct, 0.0, 0.0, 0.0, 0.0, 0.0, 60.0, 0.0, tx, re]
    }

    #[test]
    fn retry_rule_needs_enough_packets() {
        let t = 1_800_000_000;
        let live = Live::default();
        // 低流量：每 5 分钟 100 个包、重传 20%，不应判定
        let mut st = Store::new(None);
        for i in 0..12 {
            st.push("vap1", t - 3600 + i * 300, vap_row(80.0, 20.0), t);
        }
        let (f, score) = findings(&st, &live, t);
        assert!(!f.iter().any(|x| x.title.contains("重传")));
        assert_eq!(score, 100);
        // 高流量：每 5 分钟 1 万个包、重传 15%，应判定
        let mut st = Store::new(None);
        for i in 0..12 {
            st.push("vap1", t - 3600 + i * 300, vap_row(8500.0, 1500.0), t);
        }
        let (f, score) = findings(&st, &live, t);
        assert!(f.iter().any(|x| x.title == "5G 重传偏高" && x.detail.contains("15.0%")), "{:?}", f.iter().map(|x| &x.detail).collect::<Vec<_>>());
        assert_eq!(score, 90);
    }

    #[test]
    fn random_mac_bit() {
        assert!(is_random("7e:00:00:00:00:01"));
        assert!(!is_random("04:cf:8c:00:00:01"));
    }
}

// ---------- 登录 ----------

fn deny(code: StatusCode, msg: &str) -> Response {
    (code, Json(json!({ "error": msg }))).into_response()
}

/// 改动类请求的门槛：已登录 + 防跨站头。还没设置过密码的全新安装，设置接口可以先不登录（boot=true）。
fn gate(app: &App, h: &HeaderMap, boot: bool) -> Option<Response> {
    if !auth::has_csrf_header(h) {
        return Some(deny(StatusCode::FORBIDDEN, "缺少请求头"));
    }
    if boot && app.settings.read().unwrap().password.is_empty() {
        return None;
    }
    (!app.auth.valid(h)).then(|| deny(StatusCode::UNAUTHORIZED, "请先用 AP 管理员密码登录"))
}

async fn session(State(app): State<App>, h: HeaderMap) -> Json<Value> {
    Json(json!({ "logged_in": app.auth.valid(&h), "password_set": !app.settings.read().unwrap().password.is_empty() }))
}

#[derive(Deserialize)]
struct LoginQ {
    password: String,
}

async fn login(State(app): State<App>, h: HeaderMap, Json(q): Json<LoginQ>) -> Response {
    if !auth::has_csrf_header(&h) {
        return deny(StatusCode::FORBIDDEN, "缺少请求头");
    }
    let real = app.settings.read().unwrap().password.clone();
    match app.auth.login(&q.password, &real) {
        Login::Ok(t) => ([(header::SET_COOKIE, auth::set_cookie(&t))], Json(json!({ "ok": true }))).into_response(),
        Login::Wrong => deny(StatusCode::UNAUTHORIZED, "密码不对"),
        Login::Locked(s) => deny(StatusCode::TOO_MANY_REQUESTS, &format!("输错次数太多，{s} 秒后再试")),
    }
}

async fn logout(State(app): State<App>, h: HeaderMap) -> Response {
    app.auth.logout(&h);
    ([(header::SET_COOKIE, format!("{}=; Path=/; Max-Age=0", auth::COOKIE))], Json(json!({ "ok": true }))).into_response()
}

// ---------- 设置与改名 ----------

async fn settings_get(State(app): State<App>) -> Json<Value> {
    Json(app.settings.read().unwrap().public())
}

async fn settings_set(State(app): State<App>, h: HeaderMap, Json(patch): Json<Value>) -> Response {
    if let Some(r) = gate(&app, &h, true) {
        return r;
    }
    let cur = app.settings.read().unwrap().clone();
    let (next, reconnect) = match cur.apply(&patch) {
        Ok(x) => x,
        Err(e) => return deny(StatusCode::BAD_REQUEST, &e),
    };
    if let Some(d) = &app.data_dir {
        if let Err(e) = next.save(d) {
            return deny(StatusCode::INTERNAL_SERVER_ERROR, &format!("保存失败：{e}"));
        }
    }
    *app.settings.write().unwrap() = next.clone();
    if reconnect {
        let _ = app.tx.try_send(Req::Reload);
    }
    Json(json!({ "ok": true, "reconnect": reconnect, "settings": next.public() })).into_response()
}

#[derive(Deserialize)]
struct PauseQ {
    paused: bool,
}

/// 暂停 / 恢复整个采集：断开到 AP 的 telnet，不再执行任何采集命令；状态写进 config.json，重启后保持，只能手动恢复。
async fn collector_pause(State(app): State<App>, ConnectInfo(peer): ConnectInfo<SocketAddr>, h: HeaderMap, Json(q): Json<PauseQ>) -> Response {
    if let Some(r) = gate(&app, &h, false) {
        return r;
    }
    let mut next = app.settings.read().unwrap().clone();
    if next.paused_since.is_some() == q.paused {
        return Json(json!({ "ok": true, "paused_since": next.paused_since })).into_response();
    }
    next.paused_since = q.paused.then(now);
    if let Some(d) = &app.data_dir {
        if let Err(e) = next.save(d) {
            return deny(StatusCode::INTERNAL_SERVER_ERROR, &format!("保存失败：{e}"));
        }
    }
    *app.settings.write().unwrap() = next.clone();
    let msg = format!("{}采集（{}）", if q.paused { "暂停" } else { "恢复" }, peer.ip());
    app.store.write().unwrap().event(crate::store::Event { t: now(), kind: "control".into(), band: None, mac: None, msg, kts: None });
    if app.tx.send(Req::Reload).await.is_err() {
        return deny(StatusCode::INTERNAL_SERVER_ERROR, "采集任务没有响应");
    }
    Json(json!({ "ok": true, "paused_since": next.paused_since })).into_response()
}

#[derive(Deserialize)]
struct AliasQ {
    mac: String,
    name: Option<String>,
}

async fn alias(State(app): State<App>, h: HeaderMap, Json(q): Json<AliasQ>) -> Response {
    if let Some(r) = gate(&app, &h, false) {
        return r;
    }
    let mac = q.mac.to_ascii_lowercase();
    if !crate::cmds::valid_mac(&mac) {
        return deny(StatusCode::BAD_REQUEST, "MAC 格式不对");
    }
    if q.name.as_deref().is_some_and(|n| n.chars().count() > 40) {
        return deny(StatusCode::BAD_REQUEST, "名称最多 40 个字");
    }
    let mut st = app.store.write().unwrap();
    if let Err(e) = st.set_alias(&mac, q.name) {
        return deny(StatusCode::INTERNAL_SERVER_ERROR, &format!("保存失败：{e}"));
    }
    Json(json!({ "ok": true, "resolved": st.resolve(&mac) })).into_response()
}

// ---------- 控制 ----------

fn ctl_json(app: &App, s: &crate::web::CtlState) -> Value {
    let st = app.store.read().unwrap();
    let blocked: Vec<Value> = s
        .blocked
        .iter()
        .map(|b| {
            let r = st.resolve(&b.mac);
            json!({ "mac": b.mac, "name": r.name.or(Some(b.name.clone()).filter(|x| !x.is_empty())), "ip": r.ip, "random_mac": is_random(&b.mac) })
        })
        .collect();
    let plans: Vec<Value> = app.ctl.plans().iter().map(|p| json!({ "id": p.id, "at": p.at, "desc": p.desc, "op": p.op })).collect();
    json!({ "radio": s.radio, "blocked": blocked, "plans": plans })
}

#[derive(Deserialize)]
struct CtlQ {
    fresh: Option<bool>,
}

async fn ctl_state(State(app): State<App>, h: HeaderMap, Query(q): Query<CtlQ>) -> Response {
    if !app.auth.valid(&h) {
        return deny(StatusCode::UNAUTHORIZED, "请先用 AP 管理员密码登录");
    }
    match app.ctl.state(q.fresh.unwrap_or(false)).await {
        Ok(s) => Json(ctl_json(&app, &s)).into_response(),
        Err(e) => deny(StatusCode::BAD_GATEWAY, &format!("{e:#}")),
    }
}

const MAX_RESTORE_MINS: u64 = 7 * 24 * 60;

#[derive(Deserialize)]
struct RadioQ {
    band: usize,
    enable: bool,
    restore_mins: Option<u64>,
}

async fn ctl_radio(State(app): State<App>, ConnectInfo(peer): ConnectInfo<SocketAddr>, h: HeaderMap, Json(q): Json<RadioQ>) -> Response {
    if let Some(r) = gate(&app, &h, false) {
        return r;
    }
    if q.band > 1 || q.restore_mins.unwrap_or(0) > MAX_RESTORE_MINS {
        return deny(StatusCode::BAD_REQUEST, "参数不对");
    }
    let mins = if q.enable { 0 } else { q.restore_mins.unwrap_or(0) };
    match app.ctl.run(Op::Radio { band: q.band, enable: q.enable }, mins, &peer.ip().to_string()).await {
        Ok(s) => Json(ctl_json(&app, &s)).into_response(),
        Err(e) => deny(StatusCode::BAD_GATEWAY, &format!("{e:#}")),
    }
}

#[derive(Deserialize)]
struct BlockQ {
    mac: String,
    block: bool,
    restore_mins: Option<u64>,
}

async fn ctl_block(State(app): State<App>, ConnectInfo(peer): ConnectInfo<SocketAddr>, h: HeaderMap, Json(q): Json<BlockQ>) -> Response {
    if let Some(r) = gate(&app, &h, false) {
        return r;
    }
    let mac = q.mac.to_ascii_lowercase();
    if !crate::cmds::valid_mac(&mac) || q.restore_mins.unwrap_or(0) > MAX_RESTORE_MINS {
        return deny(StatusCode::BAD_REQUEST, "参数不对");
    }
    let mins = if q.block { q.restore_mins.unwrap_or(0) } else { 0 };
    match app.ctl.run(Op::Block { mac, block: q.block }, mins, &peer.ip().to_string()).await {
        Ok(s) => Json(ctl_json(&app, &s)).into_response(),
        Err(e) => deny(StatusCode::BAD_GATEWAY, &format!("{e:#}")),
    }
}

#[derive(Deserialize)]
struct CancelQ {
    id: u64,
}

async fn ctl_cancel(State(app): State<App>, h: HeaderMap, Json(q): Json<CancelQ>) -> Response {
    if let Some(r) = gate(&app, &h, false) {
        return r;
    }
    Json(json!({ "ok": app.ctl.cancel(q.id), "plans": app.ctl.plans() })).into_response()
}
