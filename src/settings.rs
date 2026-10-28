//! 可在页面上修改的设置，保存在数据目录的 config.json（权限 600）。
//!
//! 密码与解锁串只写不读：接口只返回“是否已设置”。改 AP 地址时必须同时重新给出密码和解锁串，
//! 防止有人把地址改成自己的机器，骗采集程序把已保存的口令发过去。

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub ap_host: String,
    pub password: String,
    pub unlock: String,
    /// 系统、信道、终端、射频计数
    pub fast_secs: u64,
    /// 内核日志末 40 行
    pub dmesg_secs: u64,
    /// VAP 统计、温度、SSID、设备名
    pub slow_secs: u64,
    /// 内存数据追加写盘的间隔
    pub save_mins: u64,
    /// dnsmasq 租约文件；空串或文件不存在表示不用
    pub leases_path: String,
    /// 暂停采集的时刻；None 表示正常采集。只在页面上手动恢复（2026-10-27 用户决定），不随设置提交改变。
    pub paused_since: Option<i64>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            ap_host: "192.168.1.3".into(),
            password: String::new(),
            unlock: String::new(),
            fast_secs: 30,
            dmesg_secs: 60,
            slow_secs: 300,
            save_mins: 30,
            leases_path: "/tmp/dhcp.leases".into(),
            paused_since: None,
        }
    }
}

/// (字段, 下限, 上限)。下限护住 AP：快速轮询最短 10 秒。
pub const BOUNDS: [(&str, u64, u64); 4] =
    [("fast_secs", 10, 600), ("dmesg_secs", 30, 3600), ("slow_secs", 60, 3600), ("save_mins", 5, 240)];

/// 2026-10-26 A/B 实测：30 秒一轮时扣除软中断后多约 0.24 个百分点（AP 两核总能力）。按轮次频率线性估算。
pub fn est_ap_cpu_pct(fast_secs: u64) -> f64 {
    0.24 * 30.0 / fast_secs.max(1) as f64
}

impl Settings {
    pub fn path(dir: &Path) -> PathBuf {
        dir.join("config.json")
    }

    pub fn load(dir: &Path) -> Option<Settings> {
        let b = std::fs::read(Self::path(dir)).ok()?;
        serde_json::from_slice(&b).ok()
    }

    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::create_dir_all(dir)?;
        let tmp = dir.join("config.json.tmp");
        let mut f = std::fs::OpenOptions::new().create(true).write(true).truncate(true).mode(0o600).open(&tmp)?;
        f.write_all(&serde_json::to_vec_pretty(self).unwrap_or_default())?;
        f.sync_all()?;
        std::fs::rename(&tmp, Self::path(dir))
    }

    /// 给页面看的版本：不含口令。
    pub fn public(&self) -> Value {
        json!({
            "ap_host": self.ap_host,
            "password_set": !self.password.is_empty(),
            "unlock_set": !self.unlock.is_empty(),
            "fast_secs": self.fast_secs,
            "dmesg_secs": self.dmesg_secs,
            "slow_secs": self.slow_secs,
            "save_mins": self.save_mins,
            "leases_path": self.leases_path,
            "paused_since": self.paused_since,
            "est_ap_cpu_pct": est_ap_cpu_pct(self.fast_secs),
            "bounds": BOUNDS.iter().map(|(k, lo, hi)| (k.to_string(), json!([lo, hi]))).collect::<serde_json::Map<_, _>>(),
        })
    }

    /// 合并页面提交的修改。返回（新设置，是否需要重连 AP）。
    pub fn apply(&self, patch: &Value) -> Result<(Settings, bool), String> {
        let obj = patch.as_object().ok_or("请求格式不对")?;
        let mut n = self.clone();
        let s = |k: &str| obj.get(k).and_then(|v| v.as_str()).map(|x| x.trim().to_string());
        if let Some(h) = s("ap_host") {
            if !valid_host(&h) {
                return Err("AP 地址只能是 IPv4 地址或主机名".into());
            }
            n.ap_host = h;
        }
        if let Some(p) = s("password").filter(|x| !x.is_empty()) {
            n.password = p;
        }
        if let Some(u) = s("unlock").filter(|x| !x.is_empty()) {
            n.unlock = u;
        }
        if n.ap_host != self.ap_host
            && (s("password").is_none_or(|x| x.is_empty()) || s("unlock").is_none_or(|x| x.is_empty()))
        {
            return Err("更改 AP 地址时需要同时重新输入密码和解锁串".into());
        }
        for (k, lo, hi) in BOUNDS {
            if let Some(v) = obj.get(k) {
                let v = v.as_u64().ok_or(format!("{k} 必须是整数"))?;
                if v < lo || v > hi {
                    return Err(format!("{k} 必须在 {lo}～{hi} 之间"));
                }
                match k {
                    "fast_secs" => n.fast_secs = v,
                    "dmesg_secs" => n.dmesg_secs = v,
                    "slow_secs" => n.slow_secs = v,
                    _ => n.save_mins = v,
                }
            }
        }
        if let Some(p) = s("leases_path") {
            if !p.is_empty() && !p.starts_with('/') {
                return Err("租约文件要写绝对路径，或留空不用".into());
            }
            n.leases_path = p;
        }
        let reconnect = n.ap_host != self.ap_host
            || n.password != self.password
            || n.unlock != self.unlock
            || n.fast_secs != self.fast_secs
            || n.dmesg_secs != self.dmesg_secs
            || n.slow_secs != self.slow_secs;
        Ok((n, reconnect))
    }
}

fn valid_host(h: &str) -> bool {
    !h.is_empty()
        && h.len() <= 253
        && h.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
        && !h.starts_with('-')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Settings {
        Settings { password: "pw".into(), unlock: "ul".into(), ..Default::default() }
    }

    #[test]
    fn host_change_requires_both_secrets() {
        let s = base();
        assert!(s.apply(&json!({"ap_host": "192.168.1.9"})).is_err());
        assert!(s.apply(&json!({"ap_host": "192.168.1.9", "password": "x"})).is_err());
        let (n, rc) = s.apply(&json!({"ap_host": "192.168.1.9", "password": "x", "unlock": "y"})).unwrap();
        assert!(rc && n.ap_host == "192.168.1.9" && n.password == "x");
        // 地址不变时留空口令表示不改
        let (n, rc) = s.apply(&json!({"ap_host": "192.168.1.3", "password": ""})).unwrap();
        assert!(!rc && n.password == "pw");
        assert!(s.apply(&json!({"ap_host": "a;reboot"})).is_err());
    }

    #[test]
    fn interval_bounds_and_reconnect() {
        let s = base();
        assert!(s.apply(&json!({"fast_secs": 5})).is_err());
        assert!(s.apply(&json!({"fast_secs": "30"})).is_err());
        let (n, rc) = s.apply(&json!({"fast_secs": 60})).unwrap();
        assert!(rc && n.fast_secs == 60);
        let (n, rc) = s.apply(&json!({"save_mins": 10})).unwrap();
        assert!(!rc && n.save_mins == 10);
        // 2026-10-27 之前的 config.json 带 lanwatch_url：读入时忽略
        let old: Settings = serde_json::from_str(r#"{"ap_host":"192.168.1.3","lanwatch_url":"http://192.168.1.1:9082"}"#).unwrap();
        assert_eq!(old.ap_host, "192.168.1.3");
        assert!((est_ap_cpu_pct(60) - 0.12).abs() < 1e-9);
    }

    #[test]
    fn pause_survives_reload_and_ignores_settings_patch() {
        // 加这个字段之前保存的 config.json 没有它：读出来是“正常采集”
        let old: Settings = serde_json::from_str(r#"{"ap_host":"192.168.1.3","password":"pw","unlock":"ul","fast_secs":30}"#).unwrap();
        assert_eq!(old.paused_since, None);
        let s = Settings { paused_since: Some(1_800_000_000), ..base() };
        // 提交设置不能顺带暂停或恢复
        let (n, _) = s.apply(&json!({"paused_since": null, "fast_secs": 60})).unwrap();
        assert_eq!(n.paused_since, Some(1_800_000_000));
        assert_eq!(s.public()["paused_since"], json!(1_800_000_000));
        let dir = std::env::temp_dir().join(format!("apwatch-pause-{}", std::process::id()));
        s.save(&dir).unwrap();
        assert_eq!(Settings::load(&dir).unwrap().paused_since, Some(1_800_000_000));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn public_view_hides_secrets_and_file_is_private() {
        let s = base();
        let p = s.public().to_string();
        assert!(!p.contains("\"pw\"") && !p.contains("\"ul\""));
        let dir = std::env::temp_dir().join(format!("apwatch-set-{}", std::process::id()));
        s.save(&dir).unwrap();
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(Settings::path(&dir)).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(Settings::load(&dir).unwrap(), s);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
