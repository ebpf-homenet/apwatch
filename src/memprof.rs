//! 内存与落盘剖析、新旧实现对拍（只在测试构建里存在）。
//!
//! 按主路由 2026-11-06 的规模合成 30 天采集：ap、radio0/1 每 30 秒，vap0/1 每 5 分钟，
//! 终端 39 台（22 台常在线，其余隔几天出现），与现网文件统计的行数同量级。不读真实数据。
//!
//! 运行：`cargo test --release memprofile -- --ignored --nocapture --test-threads=1`
//! 环境变量：`AP_SAVE_MINS`（落盘间隔，默认 360）、`AP_DUMP=<文件>`（写出各视图结果供对拍）。

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

pub struct Counting;
static CUR: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn grow(n: usize) {
    let c = CUR.fetch_add(n, Ordering::Relaxed) + n;
    PEAK.fetch_max(c, Ordering::Relaxed);
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            grow(l.size());
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) };
        CUR.fetch_sub(l.size(), Ordering::Relaxed);
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, l, new) };
        if !q.is_null() {
            if new >= l.size() {
                grow(new - l.size());
            } else {
                CUR.fetch_sub(l.size() - new, Ordering::Relaxed);
            }
        }
        q
    }
}

#[global_allocator]
static A: Counting = Counting;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{schema, Event, Store};
    use std::time::Instant;

    fn mib(b: usize) -> String {
        format!("{:.2} MiB", b as f64 / 1048576.0)
    }
    fn live() -> usize {
        CUR.load(Ordering::Relaxed)
    }

    struct Rng(u64);
    impl Rng {
        fn f(&mut self) -> f32 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 % 10_000) as f32 / 100.0
        }
    }

    fn sample(key: &str, r: &mut Rng) -> Vec<f32> {
        let n = schema(key).names.len();
        (0..n).map(|i| if i == 3 && r.f() < 5.0 { f32::NAN } else { r.f() }).collect()
    }

    const NOW: i64 = 1_800_000_000;

    #[test]
    #[ignore]
    fn memprofile() {
        let save = std::env::var("AP_SAVE_MINS").ok().and_then(|x| x.parse().ok()).unwrap_or(360i64) * 60;
        let dir = std::env::temp_dir().join(format!("apwatch-memprof-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let base = live();
        let mut s = Store::new(Some(dir.clone()));
        let mut r = Rng(0x9e3779b97f4a7c15);
        let clients: Vec<String> = (0..39).map(|i| format!("c:02:00:00:00:00:{i:02x}")).collect();
        let start = NOW - 30 * 86400;
        let mut last_save = start;
        let mut peak_last_day = 0usize;
        let mut writes = 0usize;
        let t0 = Instant::now();
        let mut t = start;
        while t <= NOW {
            for k in ["ap", "radio0", "radio1"] {
                let v = sample(k, &mut r);
                s.push(k, t, v, t);
            }
            if t % 300 == 0 {
                for k in ["vap0", "vap1"] {
                    let v = sample(k, &mut r);
                    s.push(k, t, v, t);
                }
            }
            let day = (t - start) / 86400;
            for (i, c) in clients.iter().enumerate() {
                if i < 22 || day % (i as i64 - 19) == 0 {
                    let v = sample(c, &mut r);
                    s.push(c, t, v, t);
                }
            }
            if t % 3600 == 0 {
                s.event(Event { t, kind: "online".into(), band: Some(1), mac: Some(clients[0][2..].into()), msg: "上线".into(), kts: None });
            }
            if t - last_save >= save {
                last_save = t;
                writes += s.flush(t).unwrap();
            }
            if t > NOW - 86400 {
                peak_last_day = peak_last_day.max(live() - base);
            }
            t += 30;
        }
        let sim = t0.elapsed();
        let before_flush = live() - base;
        writes += s.flush(NOW).unwrap();
        let after_flush = live() - base;
        let rows: usize = s.series.values().map(|x| x.rows.iter().map(|r| r.len()).sum::<usize>()).sum();
        println!(
            "落盘间隔 {} 分钟：30 天采集耗时 {:.2?}；最后一天存活峰值 {}；结束时（写盘前）{}，写盘后 {}；序列 {} 条、各档共 {} 行；写出 {} 行",
            save / 60,
            sim,
            mib(peak_last_day),
            mib(before_flush),
            mib(after_flush),
            s.series.len(),
            rows,
            writes
        );
        let disk: u64 = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.metadata().unwrap().len()).sum();
        println!("数据目录 {disk} 字节");

        let ranges = [3600, 6 * 3600, 86400, 7 * 86400, 30 * 86400];
        let keys = ["ap", "radio0", "vap1", "c:02:00:00:00:00:00", "c:02:00:00:00:00:1f"];
        let views = |s: &Store| -> Vec<serde_json::Value> {
            let mut out = Vec::new();
            for k in keys {
                for rg in ranges {
                    for pts in [100_000, 300] {
                        let q = s.series[k].query(NOW - rg, NOW, pts, schema(k).aggs);
                        let q: Vec<(i64, Vec<Option<f32>>)> =
                            q.into_iter().map(|(t, v)| (t, v.into_iter().map(|x| (!x.is_nan()).then_some(x)).collect())).collect();
                        out.push(serde_json::json!({"k": k, "range": rg, "pts": pts, "rows": q}));
                    }
                }
                out.push(serde_json::json!({"k": k, "recent": s.series[k].recent(0, 600, NOW).iter().map(|x| (!x.is_nan()).then_some(*x)).collect::<Vec<_>>()}));
            }
            out
        };
        let mem_views = views(&s);
        drop(s);
        let b2 = live();
        let t1 = Instant::now();
        let mut s2 = Store::new(Some(dir.clone()));
        let n = s2.load(NOW);
        println!("重启回放 {n} 条，耗时 {:.2?}，存活 {}", t1.elapsed(), mib(live() - b2));
        let disk_views = views(&s2);
        if let Ok(p) = std::env::var("AP_DUMP") {
            std::fs::write(format!("{p}.disk"), serde_json::to_vec(&disk_views).unwrap()).unwrap();
            std::fs::write(&p, serde_json::to_vec(&mem_views).unwrap()).unwrap();
        }
        // 已下线终端最后那个未关闭的桶从未写盘，重启后会少一格（旧版就如此，不在本次改动范围），比较时排除它
        let keep = |v: &Vec<serde_json::Value>| -> Vec<serde_json::Value> {
            v.iter().filter(|x| x["k"] != "c:02:00:00:00:00:1f").cloned().collect()
        };
        assert_eq!(keep(&mem_views), keep(&disk_views), "重启回放后的视图应与内存里的一致");
        assert_eq!(s2.events.len(), 30 * 24 + 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
