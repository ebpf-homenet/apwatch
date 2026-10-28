//! 与 AP 的 telnet 长连接：标准 login → TP-Link 保护 shell(psh) → goash 解锁 → ash。
//!
//! 每条命令用起止哨兵包裹：`echo __S7$(printf B)__;CMD;echo __E7$(printf E)__`。
//! 回显里是 `$(printf B)` 字面量，完整哨兵只会出现在真实输出中；tty 对长命令回显的折行不影响判断。

use anyhow::{bail, Context, Result};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

pub struct Session {
    s: TcpStream,
    buf: Vec<u8>,
    seq: u64,
}

pub struct Output {
    pub text: String,
    pub elapsed: Duration,
    pub bytes: usize,
}

impl Session {
    pub async fn connect(addr: &str, password: &str, unlock: &str) -> Result<Self> {
        let s = timeout(Duration::from_secs(8), TcpStream::connect(addr))
            .await
            .context("连接超时")??;
        s.set_nodelay(true).ok();
        let mut me = Session { s, buf: Vec::new(), seq: 0 };
        me.expect(&["login:"], 10).await.context("等待 login 提示")?;
        me.send("root").await?;
        me.expect(&["assword:"], 8).await.context("等待口令提示")?;
        me.send(password).await?;
        me.expect(&["# ", "$ ", "psh"], 8).await.context("等待登录完成")?;
        me.send("goash").await?;
        me.expect(&["Sign"], 8).await.context("等待 goash 挑战")?;
        me.read_for(Duration::from_millis(500)).await?;
        me.send(unlock).await?;
        let got = me.expect(&["now in ash", "Password OK"], 10).await.context("等待进入 ash")?;
        if !got.contains("ash") && !got.contains("OK") {
            bail!("goash 解锁失败");
        }
        me.read_for(Duration::from_millis(300)).await?;
        Ok(me)
    }

    async fn send(&mut self, line: &str) -> Result<()> {
        self.s.write_all(line.as_bytes()).await?;
        self.s.write_all(b"\r\n").await?;
        Ok(())
    }

    /// 读一块数据，拒绝全部 telnet 选项协商，返回去掉 IAC 后的字节数。
    async fn read_some(&mut self, wait: Duration) -> Result<usize> {
        let mut tmp = [0u8; 16384];
        let n = match timeout(wait, self.s.read(&mut tmp)).await {
            Err(_) => return Ok(0),
            Ok(r) => r?,
        };
        if n == 0 {
            bail!("AP 关闭了连接");
        }
        let mut i = 0;
        let before = self.buf.len();
        let mut replies = Vec::new();
        while i < n {
            let b = tmp[i];
            if b == 255 && i + 1 < n {
                let c = tmp[i + 1];
                if (251..=254).contains(&c) && i + 2 < n {
                    let o = tmp[i + 2];
                    match c {
                        253 => replies.extend_from_slice(&[255, 252, o]), // DO → WONT
                        251 => replies.extend_from_slice(&[255, 254, o]), // WILL → DONT
                        _ => {}
                    }
                    i += 3;
                    continue;
                }
                if c == 255 {
                    self.buf.push(255);
                }
                i += 2;
                continue;
            }
            self.buf.push(b);
            i += 1;
        }
        if !replies.is_empty() {
            self.s.write_all(&replies).await?;
        }
        Ok(self.buf.len() - before)
    }

    async fn read_for(&mut self, d: Duration) -> Result<()> {
        let end = Instant::now() + d;
        while Instant::now() < end {
            self.read_some(end - Instant::now()).await?;
        }
        Ok(())
    }

    async fn expect(&mut self, any: &[&str], secs: u64) -> Result<String> {
        let end = Instant::now() + Duration::from_secs(secs);
        loop {
            let text = String::from_utf8_lossy(&self.buf).to_string();
            if any.iter().any(|p| text.contains(p)) {
                self.buf.clear();
                return Ok(text);
            }
            let now = Instant::now();
            if now >= end {
                bail!("超时，没有等到 {:?}", any);
            }
            self.read_some(end - now).await?;
        }
    }

    /// 执行一条命令（必须来自 cmds 模块），返回哨兵之间的输出。
    pub async fn run(&mut self, cmd: &str, secs: u64) -> Result<Output> {
        self.seq += 1;
        let (sb, se) = (format!("__S{}B__", self.seq), format!("__E{}E__", self.seq));
        self.buf.clear();
        let t0 = Instant::now();
        self.send(&format!("echo __S{}$(printf B)__;{};echo __E{}$(printf E)__", self.seq, cmd, self.seq))
            .await?;
        let end = t0 + Duration::from_secs(secs);
        loop {
            // 只在新数据的尾部附近找结束哨兵，避免大输出时反复扫描整个缓冲
            let tail_from = self.buf.len().saturating_sub(64 * 1024);
            if find(&self.buf[tail_from..], se.as_bytes()).is_some() {
                break;
            }
            let now = Instant::now();
            if now >= end {
                bail!("命令超时: {cmd}");
            }
            self.read_some(end - now).await?;
        }
        let text = String::from_utf8_lossy(&self.buf).replace('\r', "");
        let start = text.find(&sb).map(|i| i + sb.len()).context("缺少起始哨兵")?;
        let stop = text.rfind(&se).context("缺少结束哨兵")?;
        let body = text[start..stop].trim_start_matches('\n').to_string();
        let bytes = self.buf.len();
        self.buf.clear();
        Ok(Output { text: body, elapsed: t0.elapsed(), bytes })
    }
}

fn find(h: &[u8], n: &[u8]) -> Option<usize> {
    h.windows(n.len()).position(|w| w == n)
}
