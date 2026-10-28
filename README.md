# apwatch

TP-Link 易展 AP（在 TL-XDR5480 易展 Turbo 版、IPQ5018 + QCN9074 上开发和实测）的观测、报表与少量控制。单个静态程序，页面内置，不依赖 CDN。

**看得到的**：每台终端的信号、信噪比、协商速率、PHY 模式、连接时长与按需读取的 MCS / OFDMA 分布；两个频段的信道繁忙率（平均与峰值）、吞吐、重传、认证；DFS 雷达避让、射频重启等内核事件；AP 自身 CPU、内存、温度；终端上线、离线、切换频段、关联握手等事件。

**能做的**：开关 2.4G / 5G、禁用 / 解禁设备（包括当前不在线的），都可以设“多少分钟后自动恢复”；设备备注与设备名册；暂停 / 恢复采集。

## 工作方式

- **采集只读**：通过一条 telnet 长连接，只执行固定白名单里的命令（`src/cmds.rs`，有单元测试守卫），AP 上不写任何文件。实测按默认 30 秒一轮采集，AP 的 CPU 多占约 0.24 个百分点。
- **控制走官方接口**：开关频段、禁用设备只调用 AP 官方管理页面用的同一套 Web 接口（每次登录、读回确认、退出），状态与官方页面 / App 一致，AP 重启不丢。改动类操作要先在本页面用 AP 管理员密码登录（12 小时会话，输错 5 次锁 60 秒），请求还要带 `X-Apw: 1` 头防跨站。
- **数据按显示需要分档存**：原始 30 秒 6 小时、2 分钟 24 小时、15 分钟 7 天、1 小时 30 天，每档保留最大值以免平均掉尖峰；内存里聚合，按保存间隔（默认 30 分钟）追加写 gzip 文件。
- **设备名**：备注由 apwatch 自己保存；默认名取 DHCP 租约里的主机名，IP 缺失时取本机 ARP 表（apwatch 跑在主路由上时最完整）。

需要：AP 的 telnet root 口令与 goash 解锁串（采集用），AP 管理员密码（控制用）。在别的型号或固件上，命令输出格式和官方接口可能不同，未测试。

## 下载

[Releases](https://github.com/ebpf-homenet/apwatch/releases) 里按架构下载 `apwatch-linux-<架构>`（musl 静态链接，直接运行），`SHA256SUMS` 是校验值。每次推送到 main 由 GitHub Actions 自动编译发布。

## 运行

```bash
apwatch --listen 0.0.0.0:9093 --data-dir /opt/apwatch/data --ap 192.168.1.2
```

| 参数 | 默认值 | 说明 |
|---|---|---|
| `--listen` | `0.0.0.0:9093` | 页面地址 |
| `--data-dir` | 不设则只存内存 | 历史数据、`config.json`（权限 600，含口令）、`aliases.json`（备注）、`names.json`（主机名与设备名册）、`schedule.json`（自动恢复计划） |
| `--ap` | `192.168.1.2` | 首次启动（还没有 `config.json`）时的 AP 地址 |
| `--cred-file` | 无 | 首次启动时从这个 `KEY=值` 文件读 `GUIDE_AP5480_PASSWORD` / `GUIDE_AP5480_UNLOCK`；同名环境变量优先 |
| `--leases` | `/tmp/dhcp.leases` | 首次启动时的 DHCP 租约文件 |

首次启动后，AP 地址、口令、解锁串、采集间隔、保存间隔、租约文件都在页面“设置”里改，保存到 `config.json`；口令只写不读，改 AP 地址时必须同时重新输入口令与解锁串。日志级别用 `RUST_LOG=apwatch=debug` 调整。

OpenWrt 上的 procd 启动脚本示例见 [apwatch.init](apwatch.init)，systemd 见 [apwatch.service](apwatch.service)。

## 构建与测试

```bash
cargo test
cargo clippy
cargo build --release --target x86_64-unknown-linux-musl
```

测试夹具在 `tests/fixtures/`，取自真实 AP 的输出，MAC 与 SSID 换成了虚构值。

## 接口

| 路径 | 内容 |
|---|---|
| `GET /` | 页面 |
| `GET /api/overview` | AP 与两个频段的当前状态、健康发现与评分、重要事件、雷达避让时段 |
| `GET /api/clients` | 实时终端列表（含 24 小时内离线的），附设备名、24 小时握手次数、近 1 小时信号 |
| `GET /api/devices` | 设备名册：连过 AP 的全部设备与只登记了备注的设备 |
| `GET /api/client?mac=&range=` | 单个终端的信号 / 速率序列和事件 |
| `GET /api/sta?mac=` | 按需读取单个终端的 MCS / OFDMA 分布（在 AP 上执行一次 `apstats -s`，10 秒内复用） |
| `GET /api/series?key=&range=&points=` | 时间序列：`radio0` `radio1` `vap0` `vap1` `ap` `c:<mac>` |
| `GET /api/events?kind=&mac=&band=&range=&limit=` | 事件 |
| `GET /api/collector` | 采集连接状态、各命令耗时与回传字节、白名单、存储规模 |
| `GET /api/session` · `POST /api/login` · `POST /api/logout` | 登录状态 / 用 AP 管理员密码登录 / 退出 |
| `GET /api/settings` · `POST /api/settings` | 读设置（不含口令）/ 改设置 |
| `POST /api/alias` | `{mac, name}` 设置备注，`name` 为空恢复默认 |
| `GET /api/ctl` | 两个频段开关、禁用名单、自动恢复计划 |
| `POST /api/ctl/radio` · `/api/ctl/block` · `/api/ctl/cancel` | 开关频段 / 禁用解禁设备 / 取消自动恢复计划 |
| `POST /api/collector/pause` | `{paused}` 暂停 / 恢复采集 |

改动类接口要登录并带 `X-Apw: 1`。`range` 可选 `1h` `6h` `24h` `7d` `30d`。
