//! 发往 AP 的全部命令。只读白名单：程序里任何发往 AP 的命令都必须经由本模块构造。
//!
//! TP-Link 的 tty 行缓冲约 255 字节，单条命令（含哨兵包裹）要远低于这个长度。

/// 系统状态：uptime、负载、墙钟、CPU 计数、内存、两个射频接口的收发字节。
pub const SYS: &str = "cat /proc/uptime /proc/loadavg;date +%s;head -1 /proc/stat;head -5 /proc/meminfo;grep ath /proc/net/dev";
/// 当前信道、频宽、发射功率。
pub const IW: &str = "iw dev ath0 info;iw dev ath1 info";
/// 客户端列表只取每台设备的首行，省掉约四分之三的输出。
pub const WLAN: [&str; 2] = [
    "wlanconfig ath0 list | grep '^[0-9a-f][0-9a-f]:'",
    "wlanconfig ath1 list | grep '^[0-9a-f][0-9a-f]:'",
];
pub const RADIO: [&str; 2] = ["apstats -r -i wifi0", "apstats -r -i wifi1"];
pub const VAP: [&str; 2] = ["apstats -v -i ath0", "apstats -v -i ath1"];
pub const THERM: [&str; 2] = ["thermaltool -i wifi0 -get", "thermaltool -i wifi1 -get"];
/// 只取 iwconfig 首行（ESSID）；第二行起含密钥，不让它离开 AP。
pub const SSID: &str = "iwconfig ath0|head -1;iwconfig ath1|head -1;cfg80211tool ath0 get_mode;cfg80211tool ath1 get_mode";
/// 内核日志增量；`-c` 会清空缓冲，绝不使用。
pub const DMESG_TAIL: &str = "dmesg | tail -n 40";
/// 连上后补一次全量日志，约 126KB，只在新会话开始时执行。
pub const DMESG_FULL: &str = "dmesg";

/// 单客户端统计。MAC 校验不通过返回 None，绝不拼接未校验的输入。
pub fn sta(mac: &str) -> Option<String> {
    valid_mac(mac).then(|| format!("apstats -s -m {}", mac.to_ascii_lowercase()))
}

pub fn valid_mac(mac: &str) -> bool {
    let b = mac.as_bytes();
    b.len() == 17
        && b.iter().enumerate().all(|(i, c)| {
            if i % 3 == 2 {
                *c == b':'
            } else {
                c.is_ascii_hexdigit()
            }
        })
}

/// 所有固定命令，供页面展示和测试使用。
pub fn all_fixed() -> Vec<&'static str> {
    let mut v = vec![SYS, IW, SSID, DMESG_TAIL, DMESG_FULL];
    v.extend(WLAN);
    v.extend(RADIO);
    v.extend(VAP);
    v.extend(THERM);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 只读守卫：每条命令里的每个程序都必须是只读程序，参数不能带写入类子命令。
    fn read_only(cmd: &str) -> bool {
        const PROGS: [&str; 12] = [
            "cat", "date", "head", "grep", "iw", "wlanconfig", "apstats", "thermaltool", "iwconfig",
            "cfg80211tool", "dmesg", "tail",
        ];
        cmd.split([';', '|']).all(|part| {
            let w: Vec<&str> = part.split_whitespace().collect();
            let Some(p) = w.first() else { return false };
            if !PROGS.contains(p) {
                return false;
            }
            match *p {
                "dmesg" => w.len() == 1,
                "iw" => w.len() == 4 && w[1] == "dev" && w[3] == "info",
                "wlanconfig" => w.len() == 3 && w[2] == "list",
                "cfg80211tool" => w.len() == 3 && w[2].starts_with("get_"),
                "thermaltool" => w.last() == Some(&"-get"),
                "iwconfig" => w.len() == 2,
                "apstats" => !w.iter().any(|a| a.starts_with("-c") || *a == "-R"),
                _ => true,
            }
        })
    }

    #[test]
    fn every_command_is_read_only() {
        for c in all_fixed() {
            assert!(read_only(c), "非只读命令: {c}");
            assert!(c.len() < 200, "命令过长: {c}");
        }
        assert!(read_only(&sta("02:00:00:00:00:11").unwrap()));
    }

    #[test]
    fn mac_validation_blocks_injection() {
        assert!(sta("02:00:00:00:00:11").is_some());
        for bad in ["02:00:00:00:00:1", "02:00:00:00:00:11;reboot", "02:00:00:00:00:1;", "$(reboot)", "", "02-00-00-00-00-11"] {
            assert!(sta(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn guard_rejects_writes() {
        for bad in ["dmesg -c", "iwpriv ath1 set_mode 11a", "cfg80211tool ath1 s_mode 1", "reboot", "wifitool ath1 x"] {
            assert!(!read_only(bad), "{bad}");
        }
    }
}
