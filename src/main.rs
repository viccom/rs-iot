#![warn(
    missing_docs,
    rust_2018_idioms,
    missing_debug_implementations,
    unreachable_pub
)]

//! rs-iot 二进制入口：解析 CLI、初始化 tracing、按子命令分发到 run/service。

mod run;
mod service;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "rs-iot",
    version,
    about = "Lightweight cross-platform industrial IoT edge framework"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// 配置文件路径（toml）。未指定则用默认值。
    #[arg(long, global = true)]
    config: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Command {
    /// 前台运行
    Run,
    /// 安装为系统服务（Windows: sc create / Linux: systemd unit）
    Install,
    /// 卸载系统服务
    Uninstall,
    /// 被 SCM 调用的服务入口（Windows only，由 install 注册的 binPath 触发）
    #[cfg(windows)]
    Service,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command.unwrap_or(Command::Run) {
        Command::Run => {
            let cfg = rs_iot::config::AppConfig::load_or_default(cli.config.as_deref())?;
            // 持有 guard 到 run 返回（main 退出时 drop → flush 故障末尾日志）；
            // ring_buffer + event_bus 注入 run_app → AppState（GET 端点 / SSE 共享）。
            let (_log_guard, log_ringbuffer, event_bus) = rs_iot::init_tracing(Some(&cfg.log));
            run::run(cfg, log_ringbuffer, event_bus).await?;
        }
        Command::Install => {
            // init_tracing(None) 恒返回 None guard（仅终端），ring_buffer/event_bus 不用丢弃
            let (_log_guard, _ring_buffer, _event_bus) = rs_iot::init_tracing(None);
            service::install()?
        }
        Command::Uninstall => {
            // init_tracing(None) 恒返回 None guard（仅终端），ring_buffer/event_bus 不用丢弃
            let (_log_guard, _ring_buffer, _event_bus) = rs_iot::init_tracing(None);
            service::uninstall()?
        }
        #[cfg(windows)]
        Command::Service => {
            // SCM 调用此入口；dispatch 内部接管 SCM 协议与生命周期。
            // 不在此 init_tracing：service_main 加载 config 后再初始化（含文件日志）。
            service::windows::dispatch()?;
        }
    }
    Ok(())
}
