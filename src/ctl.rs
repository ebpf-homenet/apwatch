//! 控制操作：经官方接口开关频段、禁用 / 解禁设备，串行执行；“多少分钟后自动恢复”的计划写盘保存，重启后照样执行。

use crate::collector::band_name;
use crate::settings::Settings;
use crate::store::{now, Event, Store};
use crate::web::{self, CtlState};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Op {
    Radio { band: usize, enable: bool },
    Block { mac: String, block: bool },
}

impl Op {
    pub fn describe(&self, st: &Store) -> String {
        match self {
            Op::Radio { band, enable } => format!("{} {}", if *enable { "打开" } else { "关闭" }, band_name(*band as u8)),
            Op::Block { mac, block } => {
                let r = st.resolve(mac);
                let who = r.name.or(r.ip).unwrap_or_else(|| mac.clone());
                format!("{} {who}", if *block { "禁用" } else { "解禁" })
            }
        }
    }

    fn inverse(&self) -> Op {
        match self {
            Op::Radio { band, enable } => Op::Radio { band: *band, enable: !enable },
            Op::Block { mac, block } => Op::Block { mac: mac.clone(), block: !block },
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Plan {
    pub id: u64,
    pub at: i64,
    pub op: Op,
    pub desc: String,
    #[serde(default)]
    pub tries: u32,
}

pub struct Ctl {
    pub settings: Arc<RwLock<Settings>>,
    pub store: Arc<RwLock<Store>>,
    dir: Option<PathBuf>,
    /// 同一时刻只做一个官方接口操作（官方页面同时只允许一个管理员会话）
    lock: Mutex<()>,
    plans: RwLock<Vec<Plan>>,
    cache: RwLock<Option<(Instant, CtlState)>>,
}

impl Ctl {
    pub fn new(settings: Arc<RwLock<Settings>>, store: Arc<RwLock<Store>>, dir: Option<PathBuf>) -> Arc<Ctl> {
        let plans = dir
            .as_ref()
            .and_then(|d| std::fs::read(d.join("schedule.json")).ok())
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Arc::new(Ctl { settings, store, dir, lock: Mutex::new(()), plans: RwLock::new(plans), cache: RwLock::new(None) })
    }

    fn creds(&self) -> (String, String) {
        let s = self.settings.read().unwrap();
        (s.ap_host.clone(), s.password.clone())
    }

    fn save_plans(&self) {
        if let Some(d) = &self.dir {
            let b = serde_json::to_vec_pretty(&*self.plans.read().unwrap()).unwrap_or_default();
            if let Err(e) = crate::store::atomic_write(d, "schedule.json", &b) {
                tracing::warn!("保存恢复计划失败：{e}");
            }
        }
    }

    pub fn plans(&self) -> Vec<Plan> {
        self.plans.read().unwrap().clone()
    }

    fn event(&self, mac: Option<String>, band: Option<u8>, msg: String) {
        self.store.write().unwrap().event(Event { t: now(), kind: "control".into(), band, mac, msg, kts: None });
    }

    /// 官方当前状态；15 秒内复用，避免每次打开页面都登录一次官方页面。
    pub async fn state(&self, fresh: bool) -> Result<CtlState> {
        if !fresh {
            if let Some((t, s)) = &*self.cache.read().unwrap() {
                if t.elapsed() < Duration::from_secs(15) {
                    return Ok(s.clone());
                }
            }
        }
        let _g = self.lock.lock().await;
        let (h, p) = self.creds();
        let s = web::state(&h, &p).await?;
        *self.cache.write().unwrap() = Some((Instant::now(), s.clone()));
        Ok(s)
    }

    async fn exec(&self, op: &Op) -> Result<CtlState> {
        let _g = self.lock.lock().await;
        let (h, p) = self.creds();
        let r = match op {
            Op::Radio { band, enable } => web::set_radio(&h, &p, *band, *enable).await,
            Op::Block { mac, block } => web::set_block(&h, &p, mac, *block).await,
        };
        if let Ok(s) = &r {
            *self.cache.write().unwrap() = Some((Instant::now(), s.clone()));
        }
        r
    }

    /// 执行一次操作；restore_mins > 0 时登记反向操作在之后执行。who 是发起请求的地址，写进事件。
    pub async fn run(&self, op: Op, restore_mins: u64, who: &str) -> Result<CtlState> {
        let desc = op.describe(&self.store.read().unwrap());
        let (mac, band) = match &op {
            Op::Radio { band, .. } => (None, Some(*band as u8)),
            Op::Block { mac, .. } => (Some(mac.clone()), None),
        };
        match self.exec(&op).await {
            Ok(s) => {
                // 同一对象之前登记的恢复计划作废，以这次操作为准
                self.plans.write().unwrap().retain(|p| !same_target(&p.op, &op));
                let mut msg = format!("{desc}（{who}）");
                if restore_mins > 0 {
                    let inv = op.inverse();
                    let at = now() + restore_mins as i64 * 60;
                    let d = inv.describe(&self.store.read().unwrap());
                    self.plans.write().unwrap().push(Plan { id: (now() as u64) << 8 | rand_byte(), at, op: inv, desc: d, tries: 0 });
                    msg.push_str(&format!("，{restore_mins} 分钟后自动恢复"));
                }
                self.save_plans();
                self.event(mac, band, msg);
                Ok(s)
            }
            Err(e) => {
                self.event(mac, band, format!("{desc} 失败：{e:#}（{who}）"));
                Err(e)
            }
        }
    }

    pub fn cancel(&self, id: u64) -> bool {
        let n = self.plans.read().unwrap().len();
        self.plans.write().unwrap().retain(|p| p.id != id);
        let changed = self.plans.read().unwrap().len() != n;
        if changed {
            self.save_plans();
        }
        changed
    }

    /// 到期计划的执行循环。失败的计划每分钟重试，最多 10 次。
    pub async fn scheduler(self: Arc<Self>) {
        let mut tick = tokio::time::interval(Duration::from_secs(15));
        loop {
            tick.tick().await;
            let due: Vec<Plan> = self.plans.read().unwrap().iter().filter(|p| p.at <= now()).cloned().collect();
            for p in due {
                let (mac, band) = match &p.op {
                    Op::Radio { band, .. } => (None, Some(*band as u8)),
                    Op::Block { mac, .. } => (Some(mac.clone()), None),
                };
                match self.exec(&p.op).await {
                    Ok(_) => {
                        self.plans.write().unwrap().retain(|x| x.id != p.id);
                        self.event(mac, band, format!("{}（到期自动恢复）", p.desc));
                    }
                    Err(e) => {
                        let mut w = self.plans.write().unwrap();
                        if let Some(x) = w.iter_mut().find(|x| x.id == p.id) {
                            x.tries += 1;
                            x.at = now() + 60;
                            if x.tries >= 10 {
                                let id = x.id;
                                w.retain(|x| x.id != id);
                                drop(w);
                                self.event(mac, band, format!("{} 自动恢复失败 10 次，已放弃：{e:#}", p.desc));
                                continue;
                            }
                        }
                        drop(w);
                        self.event(mac, band, format!("{} 自动恢复失败，1 分钟后重试：{e:#}", p.desc));
                    }
                }
                self.save_plans();
            }
        }
    }
}

fn same_target(a: &Op, b: &Op) -> bool {
    match (a, b) {
        (Op::Radio { band: x, .. }, Op::Radio { band: y, .. }) => x == y,
        (Op::Block { mac: x, .. }, Op::Block { mac: y, .. }) => x == y,
        _ => false,
    }
}

fn rand_byte() -> u64 {
    let mut b = [0u8; 1];
    let _ = std::fs::File::open("/dev/urandom").and_then(|mut f| std::io::Read::read_exact(&mut f, &mut b));
    b[0] as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inverse_and_target() {
        let r = Op::Radio { band: 1, enable: false };
        assert_eq!(r.inverse(), Op::Radio { band: 1, enable: true });
        let b = Op::Block { mac: "02:00:00:00:00:01".into(), block: true };
        assert_eq!(b.inverse(), Op::Block { mac: "02:00:00:00:00:01".into(), block: false });
        assert!(same_target(&r, &Op::Radio { band: 1, enable: true }));
        assert!(!same_target(&r, &Op::Radio { band: 0, enable: false }));
        assert!(!same_target(&r, &b));
        let j = serde_json::to_string(&Plan { id: 1, at: 2, op: b, desc: "x".into(), tries: 0 }).unwrap();
        assert!(j.contains("\"kind\":\"block\""));
        let p: Plan = serde_json::from_str(&j).unwrap();
        assert_eq!(p.op, Op::Block { mac: "02:00:00:00:00:01".into(), block: true });
    }
}
