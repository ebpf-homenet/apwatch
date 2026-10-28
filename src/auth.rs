//! 页面登录：用 AP 管理员密码换一个 12 小时有效的会话（Cookie，HttpOnly + SameSite=Strict）。
//! 改动类接口另要求请求头 `X-Apw: 1`，挡住跨站表单。连续输错 5 次锁 60 秒。

use axum::http::HeaderMap;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const COOKIE: &str = "apw";
const TTL: Duration = Duration::from_secs(12 * 3600);

#[derive(Default)]
pub struct Auth {
    sessions: Mutex<HashMap<String, Instant>>,
    fails: Mutex<(u32, Option<Instant>)>,
}

/// 等长比较，避免按耗时猜密码
fn same(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn token() -> String {
    let mut b = [0u8; 16];
    let _ = std::fs::File::open("/dev/urandom").and_then(|mut f| std::io::Read::read_exact(&mut f, &mut b));
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub enum Login {
    Ok(String),
    Wrong,
    Locked(u64),
}

impl Auth {
    pub fn login(&self, given: &str, real: &str) -> Login {
        let mut f = self.fails.lock().unwrap();
        if let Some(until) = f.1 {
            if Instant::now() < until {
                return Login::Locked((until - Instant::now()).as_secs() + 1);
            }
            *f = (0, None);
        }
        if real.is_empty() || !same(given, real) {
            f.0 += 1;
            if f.0 >= 5 {
                f.1 = Some(Instant::now() + Duration::from_secs(60));
            }
            return Login::Wrong;
        }
        *f = (0, None);
        let t = token();
        let mut s = self.sessions.lock().unwrap();
        s.retain(|_, at| at.elapsed() < TTL);
        s.insert(t.clone(), Instant::now());
        Login::Ok(t)
    }

    pub fn logout(&self, h: &HeaderMap) {
        if let Some(t) = cookie(h) {
            self.sessions.lock().unwrap().remove(&t);
        }
    }

    pub fn valid(&self, h: &HeaderMap) -> bool {
        cookie(h).is_some_and(|t| self.sessions.lock().unwrap().get(&t).is_some_and(|at| at.elapsed() < TTL))
    }
}

pub fn cookie(h: &HeaderMap) -> Option<String> {
    h.get_all("cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|kv| kv.trim().strip_prefix(&format!("{COOKIE}=")).map(|x| x.to_string()))
}

pub fn set_cookie(t: &str) -> String {
    format!("{COOKIE}={t}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}", TTL.as_secs())
}

pub fn has_csrf_header(h: &HeaderMap) -> bool {
    h.get("x-apw").and_then(|v| v.to_str().ok()) == Some("1")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn login_session_and_lockout() {
        let a = Auth::default();
        assert!(matches!(a.login("x", ""), Login::Wrong), "没设密码时不能登录");
        let Login::Ok(t) = a.login("pw", "pw") else { panic!() };
        let mut h = HeaderMap::new();
        h.insert("cookie", HeaderValue::from_str(&format!("other=1; apw={t}")).unwrap());
        assert!(a.valid(&h));
        a.logout(&h);
        assert!(!a.valid(&h));
        for _ in 0..5 {
            assert!(matches!(a.login("bad", "pw"), Login::Wrong));
        }
        assert!(matches!(a.login("pw", "pw"), Login::Locked(_)), "输错 5 次后锁定，正确密码也要等");
        assert!(same("abc", "abc") && !same("abc", "abd") && !same("abc", "ab"));
    }
}
