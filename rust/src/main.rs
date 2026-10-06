mod at;
mod auth;
mod block;
mod cell_window;
mod command;
mod conn;
mod control;
mod executor;
mod legacy_hits;
mod model;
mod ops;
mod qos;
mod screen;
mod server;
mod sms;
mod state;
mod ubus;
mod uci;
mod v2;
mod watchdog;
mod wifi;

use anyhow::Result;
use clap::Parser;
use server::App;
use std::{net::SocketAddr, path::PathBuf, time::Duration};

fn validate_listener_security(addr: SocketAddr, require_auth: bool) -> Result<()> {
    if !addr.ip().is_loopback() && !require_auth {
        anyhow::bail!(
            "refusing unauthenticated non-loopback listener {addr}; use --auth-token-file or --lan-bind"
        );
    }
    Ok(())
}

#[derive(Parser, Debug)]
#[command(name = "zwrt-datad", version = env!("DATAD_VERSION"))]
struct Args {
    #[arg(long)]
    once: bool,
    /// 已删除的 WebShell（和 2026-10 删掉的邻区采集）的旧开关，只为不让还带着它的旧启动脚本起不来，不起作用。
    #[arg(long, hide = true)]
    webshell: bool,
    #[arg(long, hide = true)]
    neighbor: bool,
    #[arg(long)]
    auth_token_file: Option<PathBuf>,
    #[arg(long)]
    lan_bind: Option<String>,
    #[arg(long, default_value_t = 9461)]
    lan_port: u16,
    #[arg(short = 'i', default_value_t = 1000)]
    interval: u64,
    #[arg(short = 'b', long = "bind", default_value = "127.0.0.1")]
    bind: String,
    #[arg(short = 'p', long = "port", default_value_t = 9460)]
    port: u16,
    #[arg(long, env = "ZWRT_DATAD_DIR", default_value = "/data/zwrt-datad")]
    data_dir: PathBuf,
}

/// 构建标记：给打包脚本认「这是本 fork 的 Rust 版、不带外部更新源」用
/// （scripts/build-docker.sh、manager 的 onboard/build-kit.sh 用 `grep -a` 查）。
/// 不参与任何 HTTP 输出；`#[used]` + main 里的 black_box 保证 LTO/strip 后仍在二进制里。
#[used]
static BUILD_MARKER: [u8; 35] = *b"ZWRT_DATAD_FORK_RUST_SELF_CONTAINED";

#[tokio::main]
async fn main() -> Result<()> {
    std::hint::black_box(&BUILD_MARKER);
    // reqwest（短信 HTTP 发送）用 rustls，进程里装一次 ring 作为默认加密实现。
    let _ = rustls::crypto::ring::default_provider().install_default();
    let raw: Vec<String> = std::env::args().collect();
    // 只读核对 socket 后端和 `ubus call` 的结果（ubus/compare.rs）；不起服务、不写文件。
    if raw.get(1).map(String::as_str) == Some("--ubus-compare") {
        std::process::exit(ubus::compare::run(&raw[2..]).await);
    }
    if raw.get(1).map(String::as_str) == Some("--compare-state-shape") {
        match model::compare_state_shape(&raw[2..]) {
            Ok(value) => {
                println!("{}", serde_json::to_string(&value)?);
                return Ok(());
            }
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(64);
            }
        }
    }
    let args = Args::parse();
    let interval = Duration::from_millis(args.interval.clamp(500, 5000));
    let _ = (
        &args.neighbor,
        &args.webshell,
        &args.auth_token_file,
        &args.lan_bind,
        args.lan_port,
    );
    let token = match args.auth_token_file {
        Some(path) => std::fs::read_to_string(path)
            .ok()
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty()),
        None => None,
    };
    let local_requires_auth = args.lan_bind.is_none() && token.is_some();
    legacy_hits::init(&args.data_dir);
    let app = App::new(args.data_dir, interval, token).await?;
    if args.once {
        app.start().await;
        println!("{}", serde_json::to_string(&app.snapshot().await)?);
        return Ok(());
    }
    let addr: SocketAddr = format!("{}:{}", args.bind, args.port).parse()?;
    validate_listener_security(addr, local_requires_auth)?;
    // 先监听、再采第一轮（P1-3）：第一轮期间 `/healthz` 就能回 503 `starting`，看门狗也已经在看。
    // 端口被占（另一个 datad 还在）时在这里就退出，不碰事务恢复。
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let lan = match &args.lan_bind {
        Some(lan_bind) => {
            let lan_addr: SocketAddr = format!("{}:{}", lan_bind, args.lan_port).parse()?;
            Some(tokio::net::TcpListener::bind(lan_addr).await?)
        }
        None => None,
    };
    server::write_pid_file();
    tokio::spawn(async {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            tick.tick().await;
            let _ = tokio::task::spawn_blocking(|| legacy_hits::flush(false)).await;
        }
    });
    let starting = app.clone();
    tokio::spawn(async move { starting.start().await });
    let result = if let Some(lan) = lan {
        tokio::try_join!(
            app.clone().serve(listener, false, false),
            app.serve(lan, true, true)
        )
        .map(|_| ())
    } else {
        app.serve(listener, local_requires_auth, false).await
    };
    // 服务停了就直接退出进程（P2-4）：从 main 返回会析构运行时，而运行时要等阻塞线程池里还在跑的
    // 任务（如卡在串口上的 AT 读写），可能一直等到 procd 的 SIGKILL。
    match result {
        Ok(()) => std::process::exit(0),
        Err(error) => {
            eprintln!("Error: {error:?}");
            std::process::exit(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_unauthenticated_non_loopback_listener() {
        assert!(validate_listener_security("0.0.0.0:9460".parse().unwrap(), false).is_err());
        assert!(validate_listener_security("192.168.0.1:9460".parse().unwrap(), false).is_err());
        assert!(validate_listener_security("127.0.0.1:9460".parse().unwrap(), false).is_ok());
        assert!(validate_listener_security("[::1]:9460".parse().unwrap(), false).is_ok());
        assert!(validate_listener_security("0.0.0.0:9460".parse().unwrap(), true).is_ok());
    }
}
