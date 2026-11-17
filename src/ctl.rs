//! 控制操作：经官方接口开关频段、禁用 / 解禁设备，串行执行；“多少分钟后自动恢复”的计划写盘保存，重启后照样执行。

use crate::collector::band_name;
use crate::settings::Settings;
use crate::store::{now, Event, Store};
use crate::web::{self, CtlState};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::{oneshot, watch, Mutex};
use tokio::task::JoinSet;

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

    fn target(&self) -> (Option<String>, Option<u8>) {
        match self {
            Op::Radio { band, .. } => (None, Some(*band as u8)),
            Op::Block { mac, .. } => (Some(mac.clone()), None),
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
    lock: Arc<Mutex<()>>,
    plans: RwLock<Vec<Plan>>,
    cache: RwLock<Option<(Instant, CtlState)>>,
    plans_dirty: AtomicBool,
    next_id: AtomicU64,
    jobs: std::sync::Mutex<Option<JoinSet<()>>>,
}

impl Ctl {
    pub fn new(settings: Arc<RwLock<Settings>>, store: Arc<RwLock<Store>>, dir: Option<PathBuf>) -> Arc<Ctl> {
        let mut plans: Vec<Plan> = dir
            .as_ref()
            .and_then(|d| std::fs::read(d.join("schedule.json")).ok())
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        plans.retain(|p| match &p.op {
            Op::Radio { band, .. } => *band < 2,
            Op::Block { mac, .. } => crate::cmds::valid_mac(mac),
        });
        let next_id = plans.iter().map(|p| p.id.saturating_add(1)).max().unwrap_or(0).max((now() as u64) << 8);
        Arc::new(Ctl {
            settings, store, dir, lock: Arc::new(Mutex::new(())), plans: RwLock::new(plans),
            cache: RwLock::new(None), plans_dirty: AtomicBool::new(false),
            next_id: AtomicU64::new(next_id), jobs: std::sync::Mutex::new(Some(JoinSet::new())),
        })
    }

    fn creds(&self) -> (String, String) {
        let s = self.settings.read().unwrap();
        (s.ap_host.clone(), s.password.clone())
    }

    // 调用方持有操作锁，保证内存和磁盘里的计划按官方操作的执行顺序提交。
    fn save_plans(&self) -> Result<()> {
        self.plans_dirty.store(true, Ordering::Relaxed);
        if let Some(d) = &self.dir {
            let b = serde_json::to_vec_pretty(&*self.plans.read().unwrap())?;
            crate::store::atomic_write(d, "schedule.json", &b).context("保存恢复计划失败")?;
        }
        self.plans_dirty.store(false, Ordering::Relaxed);
        Ok(())
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

    /// 调用方持有操作锁。官方确认写入时立即执行提交回调，随后读回状态并退出官方会话。
    async fn exec(&self, op: &Op, applied: impl FnOnce() -> Result<()> + Send) -> Result<CtlState> {
        let (h, p) = self.creds();
        let r = match op {
            Op::Radio { band, enable } => web::set_radio(&h, &p, *band, *enable, applied).await,
            Op::Block { mac, block } => web::set_block(&h, &p, mac, *block, applied).await,
        };
        *self.cache.write().unwrap() = r.as_ref().ok().map(|s| (Instant::now(), s.clone()));
        r
    }

    /// 拿到 AP 操作锁后由服务接管，HTTP 连接关闭时仍完成写盘、读回和退出官方会话。
    pub async fn submit(self: &Arc<Self>, op: Op, restore_mins: u64, who: String) -> Option<oneshot::Receiver<Result<CtlState>>> {
        let slot = self.lock.clone().lock_owned().await;
        let mut guard = self.jobs.lock().unwrap();
        let jobs = guard.as_mut()?;
        while jobs.try_join_next().is_some() {}
        let ctl = self.clone();
        let (tx, rx) = oneshot::channel();
        jobs.spawn(async move {
            let _slot = slot;
            let _ = tx.send(ctl.run(op, restore_mins, &who).await);
        });
        Some(rx)
    }

    pub fn stop_accepting(&self) -> JoinSet<()> {
        self.jobs.lock().unwrap().take().unwrap_or_default()
    }

    async fn run(&self, op: Op, restore_mins: u64, who: &str) -> Result<CtlState> {
        let desc = op.describe(&self.store.read().unwrap());
        let (mac, band) = op.target();
        let mut accepted = false;
        let r = self.exec(&op, || {
            accepted = true;
            let plan = (restore_mins > 0).then(|| {
                let inv = op.inverse();
                let at = now() + restore_mins as i64 * 60;
                let desc = inv.describe(&self.store.read().unwrap());
                let id = self.next_id.fetch_add(1, Ordering::Relaxed);
                Plan { id, at, op: inv, desc, tries: 0 }
            });
            let mut plans = self.plans.write().unwrap();
            let old_len = plans.len();
            plans.retain(|p| !same_target(&p.op, &op));
            plans.extend(plan);
            let changed = old_len != plans.len() || restore_mins > 0;
            drop(plans);
            let saved = if changed { self.save_plans() } else { Ok(()) };
            let msg = if restore_mins > 0 && saved.is_ok() {
                format!("{desc}（{who}），{restore_mins} 分钟后自动恢复")
            } else {
                format!("{desc}（{who}）")
            };
            self.event(mac.clone(), band, msg);
            saved
        }).await;
        if let Err(e) = &r {
            let reason = if accepted { "已由 AP 接受，后续确认失败" } else { "失败" };
            self.event(mac, band, format!("{desc} {reason}：{e:#}（{who}）"));
        }
        r
    }

    pub async fn cancel(&self, id: u64) -> Result<bool> {
        let _g = self.lock.lock().await;
        let mut plans = self.plans.write().unwrap();
        let n = plans.len();
        plans.retain(|p| p.id != id);
        let changed = plans.len() != n;
        drop(plans);
        if changed || self.plans_dirty.load(Ordering::Relaxed) {
            self.save_plans()?;
        }
        Ok(changed)
    }

    /// 到期计划的执行循环。失败的计划每分钟重试，最多 10 次。
    pub async fn scheduler(self: Arc<Self>, mut stop: watch::Receiver<bool>) {
        let mut tick = tokio::time::interval(Duration::from_secs(15));
        loop {
            tokio::select! {
                _ = stop.changed() => return,
                _ = tick.tick() => {}
            }
            if self.plans_dirty.load(Ordering::Relaxed) {
                let _g = self.lock.lock().await;
                if self.plans_dirty.load(Ordering::Relaxed) {
                    if let Err(e) = self.save_plans() {
                        tracing::warn!("{e:#}");
                    }
                }
            }
            let t = now();
            let due: Vec<u64> = self.plans.read().unwrap().iter().filter(|p| p.at <= t).map(|p| p.id).collect();
            for id in due {
                if *stop.borrow() {
                    return;
                }
                self.restore(id, &stop).await;
            }
        }
    }

    async fn restore(&self, id: u64, stop: &watch::Receiver<bool>) {
        let _g = self.lock.lock().await;
        if *stop.borrow() {
            return;
        }
        // 等官方操作锁期间，手动操作或取消请求可能已经替换了这个计划。
        let t = now();
        let plan = self.plans.read().unwrap().iter().find(|p| p.id == id && p.at <= t).cloned();
        let Some(p) = plan else { return };
        let (mac, band) = p.op.target();
        let result = self.exec(&p.op, || Ok(())).await;
        let msg = match result {
            Ok(_) => {
                self.plans.write().unwrap().retain(|x| x.id != id);
                if let Err(e) = self.save_plans() {
                    tracing::warn!("{e:#}");
                }
                format!("{}（到期自动恢复）", p.desc)
            }
            Err(e) => {
                let mut plans = self.plans.write().unwrap();
                let msg = if p.tries >= 9 {
                    plans.retain(|x| x.id != id);
                    format!("{} 自动恢复失败 10 次，已放弃：{e:#}", p.desc)
                } else {
                    let next = plans.iter_mut().find(|x| x.id == id).unwrap();
                    next.tries += 1;
                    next.at = now() + 60;
                    format!("{} 自动恢复失败，1 分钟后重试：{e:#}", p.desc)
                };
                drop(plans);
                if let Err(e) = self.save_plans() {
                    tracing::warn!("{e:#}");
                }
                msg
            }
        };
        self.event(mac, band, msg);
    }
}

fn same_target(a: &Op, b: &Op) -> bool {
    match (a, b) {
        (Op::Radio { band: x, .. }, Op::Radio { band: y, .. }) => x == y,
        (Op::Block { mac: x, .. }, Op::Block { mac: y, .. }) => x == y,
        _ => false,
    }
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

    #[tokio::test]
    async fn due_plan_is_rechecked_after_waiting_for_manual_operation() {
        let settings = Settings { ap_host: "127.0.0.1:1".into(), ..Default::default() };
        let store = Arc::new(RwLock::new(Store::new(None)));
        let ctl = Ctl::new(Arc::new(RwLock::new(settings)), store.clone(), None);
        let old = Plan { id: 1, at: now() - 1, op: Op::Radio { band: 1, enable: true }, desc: "旧计划".into(), tries: 0 };
        ctl.plans.write().unwrap().push(old.clone());
        let manual = ctl.lock.lock().await;
        let (_stop, stop_rx) = watch::channel(false);
        let restore = {
            let ctl = ctl.clone();
            tokio::spawn(async move { ctl.restore(old.id, &stop_rx).await })
        };
        tokio::task::yield_now().await;
        *ctl.plans.write().unwrap() = vec![Plan { id: 2, at: now() + 60, desc: "新计划".into(), ..old }];
        drop(manual);
        restore.await.unwrap();
        assert_eq!(ctl.plans()[0].id, 2);
        assert!(store.read().unwrap().events.is_empty());
    }
}
