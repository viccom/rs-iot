//! 脚本热加载监听（spec §5.3）：notify-debouncer-mini 监听 `scripts/<name>/`，
//! 去抖 200ms → 改动经 `reload_tx` 通知主循环（`Evt::Reload` → `reload_vm`）。
//!
//! watcher 为 tokio task，随实例 `CancellationToken` 终止（实例 stop → `token.cancel()`
//! → task 退出 → `Debouncer` drop 停止监听），不泄漏；runtime shutdown 时 task 被 abort，
//! 无需 `std::thread`。Windows notify 偶有事件丢失（spec §18 ReadDirectoryChangesW），
//! 去抖 + M7 REST 手动 reload 兜底。

use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// 启动热加载监听 task：监听 `script_dir` 改动，去抖 200ms → `reload_tx.send(())`。
///
/// watcher 为 tokio task，监听 `token`：实例 stop（`token.cancel()`）时 task 退出，
/// `Debouncer` drop 停止监听——修复此前 `std::thread` + `loop { sleep }` 在 stop/start
/// 循环中累积 watcher 线程泄漏的问题。runtime shutdown 时 tokio 直接 abort task（不依赖
/// cancel，对「忘记调 `token.cancel()`」的测试/退出场景更健壮）。失败仅 warn（监听不到
/// 不致命，靠 M7 REST 手动 reload 兜底）。
///
/// `Debouncer<RecommendedWatcher>: Send` 已核实（notify-debouncer-mini 0.7 auto trait），
/// 可 `move` 进要求 `Future: Send` 的 `tokio::spawn` task。
pub(super) fn spawn_watcher(
    script_dir: PathBuf,
    reload_tx: mpsc::Sender<()>,
    token: CancellationToken,
) {
    tokio::spawn(async move {
        // 同步创建 debouncer（一次性开销；对标 reload_vm 内 load_main 的同步 fs 模式）。
        // 回调在 debouncer 内部线程触发 → blocking_send 合法（非 async 上下文）。
        let mut debouncer = match notify_debouncer_mini::new_debouncer(
            Duration::from_millis(200),
            move |_evs: notify_debouncer_mini::DebounceEventResult| {
                // 任何改动事件（去抖后）→ 通知主循环 reload。
                // reload_tx drop（主循环退出）时 blocking_send 失败 → 无副作用。
                let _ = reload_tx.blocking_send(());
            },
        ) {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("[luaClient] 热加载 watcher 启动失败：{e}");
                return;
            }
        };
        if let Err(e) = debouncer
            .watcher()
            .watch(&script_dir, notify::RecursiveMode::NonRecursive)
        {
            tracing::warn!("[luaClient] 热加载 watch {script_dir:?} 失败：{e}");
            return;
        }
        // 保 debouncer 活 + 响应取消；sleep 仅占位（select! 主要等 token.cancelled）。
        // 返回 → debouncer drop → 停止监听 → task 结束。
        loop {
            tokio::select! {
                _ = token.cancelled() => break,
                _ = tokio::time::sleep(Duration::from_secs(3600)) => {}
            }
        }
    });
}
