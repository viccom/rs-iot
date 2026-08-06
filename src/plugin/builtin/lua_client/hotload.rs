//! 脚本热加载监听（spec §5.3）：notify-debouncer-mini 监听 `scripts/<name>/`，
//! 去抖 200ms → 改动经 `reload_tx` 通知主循环（`Evt::Reload` → `reload_vm`）。
//!
//! Windows notify 偶有事件丢失（spec §18 ReadDirectoryChangesW），去抖 + M7 REST 手动 reload 兜底。

use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc;

/// 启动热加载监听 task：监听 `script_dir` 改动，去抖 200ms → `reload_tx.send(())`。
///
/// `script_dir` = `scripts/<name>` 全路径。watcher 在 `spawn_blocking` task（notify 回调同步，
/// 故用 `blocking_send`）。失败仅 warn（监听不到不致命，靠 M7 REST 手动 reload 兜底）。
pub(super) fn spawn_watcher(script_dir: PathBuf, reload_tx: mpsc::Sender<()>) {
    // std::thread（非 tokio spawn_blocking）：watcher task 长驻（loop 保 debouncer 活），
    // 用 std thread 避免阻止 tokio runtime shutdown（测试 #[tokio::test] 退出顺畅）。
    // 进程退出时 std thread 随进程清理；reload_tx drop 时 blocking_send 失败（无副作用）。
    std::thread::spawn(move || {
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
        // 阻塞保 watcher 活（debouncer drop 会停监听）；task 随进程结束。
        loop {
            std::thread::sleep(Duration::from_secs(3600));
        }
    });
}
