//! AP 官方 Web 接口客户端：控制功能只走这里，与官方页面 / App 用同一套接口，配置由 dms 落盘、重启不丢。
//!
//! 协议（2026-10-27 读固件 jquery-1.10.1.js / uci.js 并实测）：
//! - 登录：POST `/` `{"method":"do","login":{"password": orgAuthPwd(密码)}}` → `stok`
//! - 读写：POST `/stok=<stok>/ds`，`method` 为 get / set / do
//! - 退出：`do` `{"system":{"logout":"null"}}`
//!
//! 每次操作单独登录、用完退出，不常驻管理员会话。

use crate::http;
use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::{json, Value};

const DICT: &str = "yLwVl0zKqws7LgKPRQ84Mdt708T1qQ3Ha7xv3H7NyU84p21BriUWBU43odz3iP4rBL3cD02KZciXTysVXiV8ngg6vL48rPJyAUw0HurW20xqxv9aYb4M9wK1Ae0wlro510qXeU07kV57fQMc8L6aLgMLwygtc0F10a0Dg70TOoouyFhdysuRMO51yY5ZlOZZLEal1h0t9YQW0Ko7oBwmCAHoic4HYbUyVeU3sfQ1xtXcPcf1aT303wAQhv66qzW";

/// 官方页面的 securityEncode：逐字符异或后查表（混淆，不是加密）。
fn security_encode(a: &str, b: &str, dict: &str) -> String {
    let (a, b, c): (Vec<u32>, Vec<u32>, Vec<char>) =
        (a.chars().map(|x| x as u32).collect(), b.chars().map(|x| x as u32).collect(), dict.chars().collect());
    (0..a.len().max(b.len()))
        .map(|p| {
            let (l, n) = match (a.get(p), b.get(p)) {
                (Some(x), Some(y)) => (*x, *y),
                (None, Some(y)) => (187, *y),
                (Some(x), None) => (*x, 187),
                (None, None) => (187, 187),
            };
            c[((l ^ n) as usize) % c.len()]
        })
        .collect()
}

pub fn org_auth_pwd(pw: &str) -> String {
    security_encode("RDpbLfCPsJZ7fiv", pw, DICT)
}

pub struct Session {
    base: String,
    stok: String,
}

fn code(v: &Value) -> i64 {
    v.get("error_code").and_then(|c| c.as_i64()).unwrap_or(-1)
}

impl Session {
    pub async fn login(host: &str, password: &str) -> Result<Session> {
        let base = format!("http://{host}");
        let body = json!({"method": "do", "login": {"password": org_auth_pwd(password)}}).to_string();
        let (st, text) = http::request(&base, "POST", "/", Some(&body), 15).await.context("登录官方页面")?;
        let v: Value = serde_json::from_str(&text).with_context(|| format!("登录响应不是 JSON（HTTP {st}）"))?;
        if code(&v) != 0 {
            bail!("官方页面拒绝登录（错误码 {}），检查设置里的密码", code(&v));
        }
        let stok = v.get("stok").and_then(|s| s.as_str()).context("登录响应没有 stok")?.to_string();
        Ok(Session { base, stok })
    }

    pub async fn ds(&self, method: &str, mut body: Value) -> Result<Value> {
        body["method"] = json!(method);
        let (_, text) = http::request(&self.base, "POST", &format!("/stok={}/ds", self.stok), Some(&body.to_string()), 30).await?;
        let v: Value = serde_json::from_str(&text).context("官方接口响应不是 JSON")?;
        if code(&v) != 0 {
            bail!("官方接口返回错误码 {}", code(&v));
        }
        Ok(v)
    }

    pub async fn logout(self) {
        let _ = self.ds("do", json!({"system": {"logout": "null"}})).await;
    }
}

pub fn ap_mac(mac: &str) -> String {
    mac.to_ascii_lowercase().replace(':', "-")
}

pub fn colon_mac(mac: &str) -> String {
    mac.to_ascii_lowercase().replace('-', ":")
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Blocked {
    pub mac: String,
    pub name: String,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct CtlState {
    /// [2.4G, 5G] 是否开启（官方配置值）
    pub radio: [bool; 2],
    pub blocked: Vec<Blocked>,
}

const SEC: [&str; 2] = ["wlan_host_2g", "wlan_host_5g"];

fn rows(v: &Value) -> Vec<Value> {
    // 表格返回 [{"host_info_3": {...}}, ...]
    v.as_array().map(|a| a.iter().filter_map(|e| e.as_object()?.values().next().cloned()).collect()).unwrap_or_default()
}

async fn read_state(s: &Session) -> Result<CtlState> {
    let w = s.ds("get", json!({"wireless": {"name": SEC}})).await?;
    let h = s.ds("get", json!({"hosts_info": {"table": ["blocked_host"]}})).await?;
    let on = |i: usize| w["wireless"][SEC[i]]["enable"].as_str().map(|x| x == "1" || x == "on").unwrap_or(false);
    let blocked = rows(&h["hosts_info"]["blocked_host"])
        .into_iter()
        .filter_map(|r| {
            let mac = colon_mac(r.get("mac")?.as_str()?);
            let name = ["name", "hostname"].iter().find_map(|k| r.get(*k)?.as_str().filter(|x| !x.is_empty())).unwrap_or("").to_string();
            Some(Blocked { mac, name })
        })
        .collect();
    Ok(CtlState { radio: [on(0), on(1)], blocked })
}

pub async fn state(host: &str, pw: &str) -> Result<CtlState> {
    let s = Session::login(host, pw).await?;
    let r = read_state(&s).await;
    s.logout().await;
    r
}

/// 开关频段并读回确认。
pub async fn set_radio(host: &str, pw: &str, band: usize, enable: bool) -> Result<CtlState> {
    let s = Session::login(host, pw).await?;
    let r = async {
        s.ds("set", json!({"wireless": {SEC[band]: {"enable": if enable { "1" } else { "0" }}}})).await?;
        let st = read_state(&s).await?;
        if st.radio[band] != enable {
            bail!("写入后读回不一致");
        }
        Ok(st)
    }
    .await;
    s.logout().await;
    r
}

/// 禁用 / 解禁设备并读回确认。官方动作需要带上名称与限速（取自终端表，离线设备取禁用名单）。
pub async fn set_block(host: &str, pw: &str, mac: &str, block: bool) -> Result<CtlState> {
    let s = Session::login(host, pw).await?;
    let r = async {
        let h = s.ds("get", json!({"hosts_info": {"table": ["host_info", "blocked_host"]}})).await?;
        let want = ap_mac(mac);
        let find = |t: &str| rows(&h["hosts_info"][t]).into_iter().find(|r| r["mac"].as_str().map(|m| m.eq_ignore_ascii_case(&want)).unwrap_or(false));
        let me = find("host_info").or_else(|| find("blocked_host")).unwrap_or(json!({}));
        let g = |k: &str, d: &str| me.get(k).and_then(|x| x.as_str()).unwrap_or(d).to_string();
        let body = json!({"hosts_info": {"set_block_flag": {
            "mac": want, "is_blocked": if block { "1" } else { "0" },
            "name": g("hostname", ""), "down_limit": g("down_limit", "0"), "up_limit": g("up_limit", "0"),
        }}});
        s.ds("do", body).await?;
        let st = read_state(&s).await?;
        let listed = st.blocked.iter().any(|b| b.mac == colon_mac(mac));
        if listed != block {
            bail!("写入后读回不一致");
        }
        Ok(st)
    }
    .await;
    s.logout().await;
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_encoding_matches_official_js() {
        // 与官方 securityEncode 逐字符对照计算：两串等长部分异或、超出部分与 187 异或，再对字典长度取模
        let a = "RDpbLfCPsJZ7fiv";
        let pw = "abc";
        let d: Vec<char> = DICT.chars().collect();
        let mut exp = String::new();
        for p in 0..a.len() {
            let l = a.as_bytes()[p] as u32;
            let n = if p < pw.len() { pw.as_bytes()[p] as u32 } else { 187 };
            exp.push(d[((l ^ n) as usize) % d.len()]);
        }
        assert_eq!(org_auth_pwd(pw), exp);
        assert_eq!(org_auth_pwd("").len(), a.len());
        // 与 2026-10-27 在真 AP 上登录成功的 Python 实现对照
        assert_eq!(org_auth_pwd("Test123!long-password-xyz"), "z7VtrnYbHHBXX87990CYyMlaE");
        assert_eq!(org_auth_pwd(&"x".repeat(20)).len(), 20);
    }

    #[test]
    fn mac_forms_and_table_rows() {
        assert_eq!(ap_mac("0A:BC:00:00:00:21"), "0a-bc-00-00-00-21");
        assert_eq!(colon_mac("0a-bc-00-00-00-21"), "0a:bc:00:00:00:21");
        let v = json!([{"blocked_host_1": {"mac": "0a-bc-00-00-00-21", "name": "laptop"}}]);
        let r = rows(&v);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0]["name"], "laptop");
    }
}
