//! 设备名与 IP 来源：dnsmasq 租约文件（主机名、IP）与本机 ARP 表（补静态 IP 设备的 IP）。都只读。

use crate::store::Name;

/// `/proc/net/arp`：`IP 硬件类型 标志 MAC 掩码 网卡`。只取已解析（标志含 0x2）的条目。
pub fn parse_arp(text: &str) -> Vec<(String, String)> {
    text.lines()
        .skip(1)
        .filter_map(|l| {
            let w: Vec<&str> = l.split_whitespace().collect();
            let flags = u32::from_str_radix(w.get(2)?.trim_start_matches("0x"), 16).ok()?;
            let mac = w.get(3)?.to_ascii_lowercase();
            (flags & 0x2 != 0 && crate::cmds::valid_mac(&mac) && mac != "00:00:00:00:00:00").then(|| (mac, w[0].to_string()))
        })
        .collect()
}

/// dnsmasq 租约：`到期时间 MAC IP 主机名 客户端ID`，没有主机名时写 `*`。
pub fn parse_leases(text: &str) -> Vec<(String, Name)> {
    text.lines()
        .filter_map(|l| {
            let w: Vec<&str> = l.split_whitespace().collect();
            let mac = w.get(1)?.to_ascii_lowercase();
            if !crate::cmds::valid_mac(&mac) {
                return None;
            }
            let ip = w.get(2)?.to_string();
            let name = w.get(3).filter(|h| **h != "*").map(|h| h.to_string()).unwrap_or_default();
            Some((mac, Name { name, ip }))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses_arp_complete_entries_only() {
        let t = "IP address       HW type     Flags       HW address            Mask     Device
192.168.1.155     0x1         0x2         02:00:00:00:00:02     *        br-lan
192.168.1.77      0x1         0x0         00:00:00:00:00:00     *        br-lan
192.168.1.78      0x1         0x6         02:00:00:00:00:03     *        br-lan
";
        let v = super::parse_arp(t);
        assert_eq!(v, vec![("02:00:00:00:00:02".into(), "192.168.1.155".into()), ("02:00:00:00:00:03".into(), "192.168.1.78".into())]);
    }

    #[test]
    fn parses_dnsmasq_leases() {
        let t = "1793102913 02:00:00:00:00:0A 192.168.1.134 lamp-1 *\n1793102913 02:00:00:00:00:0b 192.168.1.9 * 01:02\nbroken line\n";
        let v = super::parse_leases(t);
        assert_eq!(v.len(), 2);
        assert_eq!((v[0].0.as_str(), v[0].1.name.as_str(), v[0].1.ip.as_str()), ("02:00:00:00:00:0a", "lamp-1", "192.168.1.134"));
        assert_eq!(v[1].1.name, "");
    }
}
