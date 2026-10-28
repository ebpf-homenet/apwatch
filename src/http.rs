//! 最小 HTTP/1.0 客户端：只用于 AP 官方页面（明文 HTTP），避免引入 TLS 依赖。

use anyhow::{bail, Context, Result};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

/// url 形如 `http://host[:port]`；返回（状态码，响应体）。
pub async fn request(base: &str, method: &str, path: &str, json: Option<&str>, secs: u64) -> Result<(u16, String)> {
    let host = base.strip_prefix("http://").context("只支持 http://")?.trim_end_matches('/');
    let addr = if host.contains(':') { host.to_string() } else { format!("{host}:80") };
    let mut req = format!("{method} {path} HTTP/1.0\r\nHost: {host}\r\nConnection: close\r\n");
    if let Some(b) = json {
        req.push_str(&format!("Content-Type: application/json; charset=UTF-8\r\nContent-Length: {}\r\n", b.len()));
    }
    req.push_str("\r\n");
    if let Some(b) = json {
        req.push_str(b);
    }
    let raw = timeout(Duration::from_secs(secs), async {
        let mut s = TcpStream::connect(&addr).await?;
        s.write_all(req.as_bytes()).await?;
        let mut buf = Vec::new();
        s.read_to_end(&mut buf).await?;
        anyhow::Ok(buf)
    })
    .await
    .with_context(|| format!("{addr} 超时"))??;
    let text = String::from_utf8_lossy(&raw);
    let (head, body) = text.split_once("\r\n\r\n").context("HTTP 响应格式不对")?;
    let code: u16 = head.split_whitespace().nth(1).and_then(|c| c.parse().ok()).context("HTTP 状态行不对")?;
    if !head.starts_with("HTTP/1.") {
        bail!("不是 HTTP 响应");
    }
    Ok((code, body.to_string()))
}
