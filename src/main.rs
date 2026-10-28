//! apwatch：TL-XDR5480 无线 AP 观测、报表与少量控制。规格见 SPEC.md。

mod api;
mod auth;
mod cmds;
mod collector;
mod ctl;
mod http;
mod names;
mod parse;
mod settings;
mod store;
mod telnet;
mod web;

use anyhow::{Context, Result};
use clap::Parser;
use collector::Live;
use settings::Settings;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use store::{now, Store};

#[derive(Parser)]
#[command(about = "TL-XDR5480 无线 AP 观测、报表与少量控制")]
struct Args {
    /// 页面监听地址
    #[arg(long, default_value = "0.0.0.0:9093")]
    listen: String,
    /// 数据目录（历史数据、设置、自定义名称）；不给则只存内存
    #[arg(long)]
    data_dir: Option<PathBuf>,
    /// 首次启动、还没有 config.json 时的 AP 地址
    #[arg(long, default_value = "192.168.1.3")]
    ap: String,
    /// 首次启动时读取 GUIDE_AP5480_PASSWORD / GUIDE_AP5480_UNLOCK 的文件（KEY=值 格式）；同名环境变量优先。都没有时到页面“设置”里填
    #[arg(long)]
    cred_file: Option<PathBuf>,
    /// 首次启动时的 DHCP 租约文件
    #[arg(long, default_value = "/tmp/dhcp.leases")]
    leases: String,
}

fn creds(file: Option<&PathBuf>) -> (Option<String>, Option<String>) {
    let mut pw = std::env::var("GUIDE_AP5480_PASSWORD").ok();
    let mut un = std::env::var("GUIDE_AP5480_UNLOCK").ok();
    if pw.is_none() || un.is_none() {
        if let Some(text) = file.and_then(|f| std::fs::read_to_string(f).ok()) {
            for line in text.lines() {
                let Some((k, v)) = line.trim().split_once('=') else { continue };
                let v = v.trim().trim_matches(|c| c == '"' || c == '\'').to_string();
                match k.trim().trim_start_matches("export ").trim() {
                    "GUIDE_AP5480_PASSWORD" if pw.is_none() => pw = Some(v),
                    "GUIDE_AP5480_UNLOCK" if un.is_none() => un = Some(v),
                    _ => {}
                }
            }
        }
    }
    (pw, un)
}

/// 有 config.json 就用它；没有则用命令行与凭据文件生成一份并保存。
fn initial_settings(a: &Args) -> Settings {
    if let Some(s) = a.data_dir.as_deref().and_then(Settings::load) {
        return s;
    }
    let (pw, un) = creds(a.cred_file.as_ref());
    let s = Settings {
        ap_host: a.ap.clone(),
        password: pw.unwrap_or_default(),
        unlock: un.unwrap_or_default(),
        leases_path: a.leases.clone(),
        ..Default::default()
    };
    if let Some(d) = &a.data_dir {
        if let Err(e) = s.save(d) {
            tracing::warn!("保存初始设置失败：{e}");
        }
    }
    s
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "apwatch=info".into()))
        .init();
    let a = Args::parse();
    let settings = Arc::new(RwLock::new(initial_settings(&a)));
    if settings.read().unwrap().password.is_empty() {
        tracing::warn!("还没有 AP 密码：打开页面的“设置”填写");
    }
    let mut st = Store::new(a.data_dir.clone());
    let t = now();
    st.prune_files(t);
    let n = st.load(t);
    tracing::info!("回放历史记录 {n} 条");
    let store = Arc::new(RwLock::new(st));
    let live = Arc::new(RwLock::new(Live { started: t, ..Default::default() }));
    let (tx, rx) = tokio::sync::mpsc::channel(8);
    tokio::spawn(collector::run(settings.clone(), store.clone(), live.clone(), rx));
    let ctl = ctl::Ctl::new(settings.clone(), store.clone(), a.data_dir.clone());
    tokio::spawn(ctl.clone().scheduler());

    // 按设置的保存间隔追加写盘；每天清理一次过期文件
    let (fs, fset) = (store.clone(), settings.clone());
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
        let (mut last_save, mut last_prune) = (now(), now());
        loop {
            tick.tick().await;
            let t = now();
            let every = fset.read().unwrap().save_mins as i64 * 60;
            if t - last_save >= every {
                last_save = t;
                if let Err(e) = fs.write().unwrap().flush(t) {
                    tracing::warn!("写盘失败：{e}");
                }
            }
            if t - last_prune >= 86400 {
                last_prune = t;
                fs.read().unwrap().prune_files(t);
            }
        }
    });

    let app = api::router(api::App {
        store: store.clone(),
        live,
        tx,
        settings,
        ctl,
        auth: Arc::new(auth::Auth::default()),
        data_dir: a.data_dir.clone(),
    });
    let listener = tokio::net::TcpListener::bind(&a.listen).await.with_context(|| format!("监听 {}", a.listen))?;
    tracing::info!("页面 http://{}/", a.listen);
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(shutdown())
        .await?;
    let n = store.write().unwrap().flush(now())?;
    tracing::info!("退出前写盘 {n} 条");
    Ok(())
}

async fn shutdown() {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}
