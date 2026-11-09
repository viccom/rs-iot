//! 脚本热加载监听（spec §5.3）：notify 监听 `scripts/<name>/`，事件过滤 +
//! 200ms 静默去抖 → 改动经 `reload_tx` 通知主循环（`Evt::Reload` → `reload_vm`）。
//!
//! **必须过滤 `EventKind::Access`**（2026-11-09 Linux 实测回归）：inotify 对目录
//! watch 会上报子文件的 `IN_OPEN`/`IN_ACCESS`（notify 映射为 Access 事件）；
//! reload 自身读 `main.lua` 会再触发事件，形成「读 → 事件 → reload → 读」自激
//! 循环（实测 `vm_restart_count` 以 ~10/s 增长、`runtime_devices` 被反复清空）。
//! Windows `ReadDirectoryChangesW` 不上报访问事件，过滤对它无行为变化。
//!
//! 另有全局 `_lib/` watcher（T10/Q15，[`spawn_global_lib_watcher`]）：站点共享库
//! 变更影响**所有** luaClient 实例 → 进程级单例，Recursive 监听 `scripts/_lib/`，
//! 去抖后全实例 reload（`reload::reload_all_lua_instances`）。
//!
//! watcher 为 tokio task，随实例 `CancellationToken` 终止（实例 stop → `token.cancel()`
//! → task 退出 → `RecommendedWatcher` drop 停止监听），不泄漏；runtime shutdown 时
//! task 被 abort，无需 `std::thread`。Windows notify 偶有事件丢失（spec §18
//! ReadDirectoryChangesW），去抖 + M7 REST 手动 reload 兜底。

use notify::{Event, EventKind, RecursiveMode, Watcher};
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// 去抖静默窗：最后一次事件后再等 200ms 无新事件才通知（编辑器一次保存常连发
/// 多个事件，立即 reload 会读到写了一半的文件）。
const DEBOUNCE: Duration = Duration::from_millis(200);

/// 监听目录改动 → 过滤 Access 事件 → 去抖 → `reload_tx`。
///
/// `create_dir`：watch 前自建目录（`_lib/` 可能尚不存在；notify watch 不存在的
/// 路径直接失败且此后保存也不触发）；`mode`：`_lib/` 多级子目录用 Recursive，
/// 单脚本目录用 NonRecursive。
///
/// watcher 为 tokio task，监听 `token`：实例 stop（`token.cancel()`）时 task 退出，
/// `RecommendedWatcher` drop 停止监听——修复此前 `std::thread` + `loop { sleep }`
/// 在 stop/start 循环中累积 watcher 线程泄漏的问题。runtime shutdown 时 tokio
/// 直接 abort task（不依赖 cancel，对「忘记调 `token.cancel()`」的测试/退出场景更
/// 健壮）。失败仅 warn（监听不到不致命，靠 M7 REST 手动 reload 兜底）。
fn spawn_dir_watcher(
    dir: PathBuf,
    mode: RecursiveMode,
    create_dir: bool,
    reload_tx: mpsc::Sender<()>,
    token: CancellationToken,
) {
    tokio::spawn(async move {
        if create_dir {
            if let Err(e) = std::fs::create_dir_all(&dir) {
                tracing::warn!("[lua] watch 目录创建失败，watcher 未启动：{e}");
                return;
            }
        }
        let (evt_tx, mut evt_rx) = mpsc::channel::<()>(8);
        // 回调在 notify 内部线程触发 → blocking_send 合法（非 async 上下文）。
        // Access（Open/Read/Close）是热加载自身读脚本的副产物，不是变更信号。
        let mut watcher =
            match notify::recommended_watcher(move |res: notify::Result<Event>| match res {
                Ok(ev) if !matches!(ev.kind, EventKind::Access(_)) => {
                    let _ = evt_tx.blocking_send(());
                }
                Ok(_) => {}
                Err(e) => tracing::warn!("[lua] 热加载 watcher 事件错误：{e}"),
            }) {
                Ok(w) => w,
                Err(e) => {
                    tracing::warn!("[lua] 热加载 watcher 启动失败：{e}");
                    return;
                }
            };
        if let Err(e) = watcher.watch(&dir, mode) {
            tracing::warn!("[lua] 热加载 watch {dir:?} 失败：{e}");
            return;
        }
        if create_dir {
            tracing::info!("[lua] _lib 全局 watcher 启动（{dir:?}）");
        }
        // 保 watcher 活 + 去抖：首个变更事件后等 200ms 静默（期间新事件顺延），
        // 再通知主循环；返回 → watcher drop → 停止监听 → task 结束。
        loop {
            tokio::select! {
                _ = token.cancelled() => break,
                Some(()) = evt_rx.recv() => {
                    loop {
                        tokio::select! {
                            _ = token.cancelled() => return,
                            r = evt_rx.recv() => {
                                if r.is_none() {
                                    return;
                                }
                            }
                            _ = tokio::time::sleep(DEBOUNCE) => break,
                        }
                    }
                    if reload_tx.send(()).await.is_err() {
                        break;
                    }
                }
            }
        }
    });
}

/// 启动热加载监听 task：监听 `script_dir` 改动，去抖 200ms → `reload_tx.send(())`。
pub(crate) fn spawn_watcher(
    script_dir: PathBuf,
    reload_tx: mpsc::Sender<()>,
    token: CancellationToken,
) {
    spawn_dir_watcher(
        script_dir,
        RecursiveMode::NonRecursive,
        false,
        reload_tx,
        token,
    );
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
    spawn_dir_watcher(lib_dir, RecursiveMode::Recursive, true, reload_tx, token);
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
