//! 脚本热加载监听（spec §5.3）：notify-debouncer-mini 监听 `scripts/<name>/`，
//! 去抖 200ms → 改动经 `reload_tx` 通知主循环（`Evt::Reload` → `reload_vm`）。
//!
//! 另有全局 `_lib/` watcher（T10/Q15，[`spawn_global_lib_watcher`]）：站点共享库
//! 变更影响**所有** luaClient 实例 → 进程级单例，Recursive 监听 `scripts/_lib/`，
//! 去抖后全实例 reload（`reload::reload_all_lua_instances`）。
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
pub(crate) fn spawn_watcher(
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

/// 全局 `_lib/` 站点共享库 watcher 底层（T10/Q15）：Recursive 监听 `lib_dir`
/// （`scripts/_lib/`——共享库按 `<org>/<...>/` 分多级子目录，须递归），去抖 200ms →
/// `reload_tx`。channel 出口（无 lux/mgr 依赖），可测核心。
///
/// 与 per-instance [`spawn_watcher`] 的差异：① `Recursive`（多级子目录）；
/// ② watch 前自建 `lib_dir`——站点尚无共享库时 `_lib/` 不存在，notify watch 直接
/// 失败且此后保存也不触发，先建空目录（无害）保证 watch 成立。`_lib/` 运行中被整
/// 目录删除的极端场景不恢复（快照由 REST 版本管理兜底）。
pub(crate) fn spawn_lib_watcher(
    lib_dir: PathBuf,
    reload_tx: mpsc::Sender<()>,
    token: CancellationToken,
) {
    tokio::spawn(async move {
        if let Err(e) = std::fs::create_dir_all(&lib_dir) {
            tracing::warn!("[lua] _lib 目录创建失败，全局 watcher 未启动：{e}");
            return;
        }
        let mut debouncer = match notify_debouncer_mini::new_debouncer(
            Duration::from_millis(200),
            move |_evs: notify_debouncer_mini::DebounceEventResult| {
                // 任何改动事件（去抖后）→ 通知 reload；tx drop（下游退出）时无副作用。
                let _ = reload_tx.blocking_send(());
            },
        ) {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("[lua] _lib watcher 启动失败：{e}");
                return;
            }
        };
        if let Err(e) = debouncer
            .watcher()
            .watch(&lib_dir, notify::RecursiveMode::Recursive)
        {
            tracing::warn!("[lua] _lib watch {lib_dir:?} 失败：{e}");
            return;
        }
        tracing::info!("[lua] _lib 全局 watcher 启动（{lib_dir:?}，Recursive）");
        // 保 debouncer 活 + 响应取消（同 spawn_watcher）。
        loop {
            tokio::select! {
                _ = token.cancelled() => break,
                _ = tokio::time::sleep(Duration::from_secs(3600)) => {}
            }
        }
    });
}

/// `_lib/` 全局 watcher 组合入口（Q15，计划 T10）：变更（去抖后）→
/// [`reload_all_lua_instances`](crate::plugin::lua_core::reload::reload_all_lua_instances)
/// 全实例 stop+start 重走 require。
///
/// **落点与单例保证**：`_lib/` 变更影响所有 luaClient 实例，watcher 是进程级而非
/// per-instance——由编排层（`run_app_with_shutdown`）**单点调用一次**保证全局唯一，
/// 随进程 shutdown token 存活（无需 OnceLock：无多入口启动场景）。`scripts_root`
/// 取默认 SCRIPTS_ROOT（REST save 同基；per-instance `scripts_root` 覆盖不在
/// watcher 范围，与 REST 侧一致）。
pub(crate) fn spawn_global_lib_watcher(
    scripts_root: PathBuf,
    lux: lux::EmbeddedClient,
    mgr: std::sync::Arc<crate::plugin::instance::InstanceManager>,
    token: CancellationToken,
) {
    let (tx, mut rx) = mpsc::channel::<()>(8);
    spawn_lib_watcher(scripts_root.join("_lib"), tx, token.clone());
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = token.cancelled() => break,
                Some(()) = rx.recv() => {
                    let results =
                        crate::plugin::lua_core::reload::reload_all_lua_instances(&lux, &mgr).await;
                    tracing::info!(
                        "[lua] _lib 变更 → reload {} 个 luaClient 实例（Q15 全实例策略）",
                        results.len()
                    );
                }
                // None（watcher 退出/失败）忽略，不忙循环——token 仍会结束本 task。
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    /// 唯一临时目录（防并行测试写 temp_dir 互相触发 notify 事件，见
    /// lua_client/tests.rs watcher_exits_on_cancel 的同类注释）。
    fn fresh_dir(tag: &str) -> PathBuf {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let p =
            std::env::temp_dir().join(format!("lua-libwatch-{tag}-{}-{uniq}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    /// T10 验证意图（Q15 触发面）：`_lib/<org>/` 下写 .lua → Recursive 监听命中 →
    /// 去抖后 reload_tx 收到通知。真 notify 事件（Windows ReadDirectoryChangesW，
    /// spec §18 偶有丢失），5s 超时容忍去抖与调度抖动。
    #[tokio::test]
    async fn lib_watcher_notifies_on_lib_write() {
        let root = fresh_dir("write");
        let (tx, mut rx) = mpsc::channel::<()>(8);
        let token = CancellationToken::new();
        spawn_lib_watcher(root.join("_lib"), tx, token.clone());
        // 等 watcher 就绪（task 调度 + debouncer 创建 + watch 注册 + 自建 _lib）。
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            root.join("_lib").exists(),
            "watcher 应自建 _lib 目录（watch 前置）"
        );
        std::fs::create_dir_all(root.join("_lib/viccom")).unwrap();
        std::fs::write(root.join("_lib/viccom/hex.lua"), "return {}").unwrap();
        let r = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await;
        token.cancel();
        let _ = std::fs::remove_dir_all(&root);
        assert!(
            matches!(r, Ok(Some(()))),
            "_lib 写入应触发 reload 通知，实得 {r:?}"
        );
    }

    /// T10 验证意图（生命周期）：全局 watcher 随 token.cancel 退出（同 per-instance
    /// watcher_exits_on_cancel 回归语义：task 退出 → tx drop → recv None）。
    #[tokio::test]
    async fn lib_watcher_exits_on_cancel() {
        let root = fresh_dir("cancel");
        let (tx, mut rx) = mpsc::channel::<()>(8);
        let token = CancellationToken::new();
        spawn_lib_watcher(root.join("_lib"), tx, token.clone());
        tokio::time::sleep(Duration::from_millis(200)).await;
        token.cancel();
        let r = tokio::time::timeout(Duration::from_millis(500), rx.recv()).await;
        let _ = std::fs::remove_dir_all(&root);
        assert!(
            matches!(r, Ok(None)),
            "应随 token.cancel 退出（tx drop → recv None），实得 {r:?}"
        );
    }
}
