//! 脚本热加载监听（spec §5.3）：notify 原生 watcher 监听 `scripts/<name>/`，
//! **写类事件**（Modify/Create/Remove）去抖 200ms → `reload_tx` 通知主循环
//! （`Evt::Reload` → `reload_vm`）。
//!
//! **为何不用 notify-debouncer-mini**（2026-10-26 热加载风暴实证）：mini 0.7 有意丢弃
//! 事件类别（`DebouncedEventKind` 仅 Any/AnyContinuous），而 notify 8 的 inotify 订阅
//! 掩码（0xfee）含 IN_OPEN——`load_main` 每次 reload 都要**读** main.lua，读取产生的
//! OPEN/ACCESS 事件被当作改动回灌，形成「reload → 读脚本 → IN_OPEN → 再 reload」
//! 自激风暴：实测 4.2 万次/日/实例 + 日志洪水，且 VM 级替换反复清空脚本全局状态
//! （luaTransform 累加器 Frozen 在 base+单次增量）。本文件改用原生事件 +
//! `is_write_event` 过滤 + select! 尾沿去抖，类别在源头被滤掉，风暴不可能成立。
//!
//! 另有全局 `_lib/` watcher（T10/Q15，[`spawn_global_lib_watcher`]）：站点共享库
//! 变更影响**所有** luaClient 实例 → 进程级单例，Recursive 监听 `scripts/_lib/`，
//! 去抖后全实例 reload（`reload::reload_all_lua_instances`）。
//!
//! watcher 为 tokio task，随实例 `CancellationToken` 终止（实例 stop → `token.cancel()`
//! → task 退出 → `RecommendedWatcher` drop 停止监听），不泄漏；runtime shutdown 时
//! task 被 abort，无需 `std::thread`。Windows notify 偶有事件丢失（spec §18
//! ReadDirectoryChangesW），去抖 + M7 REST 手动 reload 兜底。

use std::path::PathBuf;
use std::time::Duration;
use notify::Watcher as _;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// 去抖窗口（改动静默 200ms 后放行——尾沿去抖，连续写自动顺延）。
const DEBOUNCE: Duration = Duration::from_millis(200);

/// 写类事件判定：只放行内容/名称/元数据变更。Access（open/read/close）是 `load_main`
/// 读 main.lua 自身产生的（notify 8 订阅掩码含 IN_OPEN），放行即自激 reload 风暴；
/// Other（未知类别）宁漏勿误——漏靠 REST 手动 reload 兜底，误则是 VM 状态清零。
fn is_write_event(ev: &notify::Event) -> bool {
    matches!(
        ev.kind,
        notify::EventKind::Modify(_) | notify::EventKind::Create(_) | notify::EventKind::Remove(_)
    )
}

/// 启动热加载监听 task：监听 `script_dir`，**写类事件**去抖 200ms → `reload_tx.send(())`。
///
/// watcher 为 tokio task，监听 `token`：实例 stop（`token.cancel()`）时 task 退出，
/// `RecommendedWatcher` drop 停止监听。失败仅 warn（监听不到不致命，靠 M7 REST 手动
/// reload 兜底）。
///
/// notify 回调在 watcher 内部线程触发（unbounded send 同步入队，见
/// `watch_debounce_loop`）；`RecommendedWatcher: Send` 已核实（notify 8 auto
/// trait），可 move 进 tokio task。
pub(crate) fn spawn_watcher(
    script_dir: PathBuf,
    reload_tx: mpsc::Sender<()>,
    token: CancellationToken,
) {
    tokio::spawn(watch_debounce_loop(
        script_dir,
        notify::RecursiveMode::NonRecursive,
        false,
        reload_tx,
        token,
        "luaClient",
    ));
}

/// 全局 `_lib/` 站点共享库 watcher 底层（T10/Q15）：Recursive 监听 `lib_dir`
/// （`scripts/_lib/`——共享库按 `<org>/<...>/` 分多级子目录，须递归），去抖 200ms →
/// `reload_tx`。channel 出口（无 lux/mgr 依赖），可测核心。
///
/// watch 前自建 `lib_dir`——站点尚无共享库时 `_lib/` 不存在，notify watch 直接失败且
/// 此后保存也不触发，先建空目录（无害）保证 watch 成立。`_lib/` 运行中被整目录删除
/// 的极端场景不恢复（快照由 REST 版本管理兜底）。
pub(crate) fn spawn_lib_watcher(
    lib_dir: PathBuf,
    reload_tx: mpsc::Sender<()>,
    token: CancellationToken,
) {
    tokio::spawn(watch_debounce_loop(
        lib_dir,
        notify::RecursiveMode::Recursive,
        true,
        reload_tx,
        token,
        "lua",
    ));
}

/// 两个 watcher 的共用主体：建 watcher → 注册监听 → 事件过滤 + 尾沿去抖循环。
/// `ensure_dir`（仅 _lib）= watch 前自建目录；`tag` = 日志前缀（luaClient/_lib）。
async fn watch_debounce_loop(
    dir: PathBuf,
    mode: notify::RecursiveMode,
    ensure_dir: bool,
    reload_tx: mpsc::Sender<()>,
    token: CancellationToken,
    tag: &str,
) {
    if ensure_dir {
        if let Err(e) = std::fs::create_dir_all(&dir) {
            tracing::warn!("[lua] _lib 目录创建失败，全局 watcher 未启动：{e}");
            return;
        }
    }
    // unbounded：notify 回调线程只做入队不阻塞；有界满容时 blocking_send 失败会
    // 丢事件——突发写（如解压大目录进 _lib）的**末**事件被丢即整轮 reload 漏触发。
    // fs 事件体量小且瞬时，unbounded 无实际内存风险（审查 2026-10-27）。
    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<notify::Result<notify::Event>>();
    let mut watcher = match notify::recommended_watcher(move |res| {
        // notify 回调线程：unbounded send 同步不阻塞；接收端 drop（task 退出）时
        // 失败 → 无副作用。
        let _ = ev_tx.send(res);
    }) {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!("[{tag}] 热加载 watcher 启动失败：{e}");
            return;
        }
    };
    if let Err(e) = watcher.watch(&dir, mode) {
        tracing::warn!("[{tag}] 热加载 watch {dir:?} 失败：{e}");
        return;
    }
    if ensure_dir {
        tracing::info!("[lua] _lib 全局 watcher 启动（{dir:?}，Recursive）");
    }
    let mut pending = false;
    loop {
        tokio::select! {
            _ = token.cancelled() => break,
            r = ev_rx.recv() => match r {
                Some(Ok(ev)) => {
                    if is_write_event(&ev) {
                        pending = true; // 尾沿去抖：新写事件不断 → 顺延到静默 200ms
                    }
                }
                Some(Err(e)) => tracing::warn!("[{tag}] watch 事件错误：{e}"),
                None => break, // watcher 侧 channel 关闭
            },
            _ = tokio::time::sleep(DEBOUNCE), if pending => {
                // async 上下文 → 必须 .await（blocking_send 仅限 notify 回调线程）
                let _ = reload_tx.send(()).await;
                pending = false;
            }
        }
    }
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

    /// 热加载风暴回归（2026-10-26 实证）：**读取**监听目录内文件不得触发 reload
    /// （notify 8 掩码含 IN_OPEN，load_main 读 main.lua 的 open/access 事件曾被当作
    /// 改动 → 「reload→读→再 reload」自激 4.2 万次/日/实例）；**写入**仍须触发。
    ///
    /// 结构（审查 2026-10-27 加固）：先写→等通知证明 watcher 已注册存活，再做读
    /// 风暴——否则慢机上 spawn 后固定 sleep 可能还没注册完就读完，"读取静默"断言
    /// 空转通过（没观测到任何事件当然静默）。
    #[tokio::test]
    async fn watcher_ignores_reads_notifies_on_write() {
        let dir = fresh_dir("readstorm");
        std::fs::create_dir_all(&dir).unwrap();
        let main = dir.join("main.lua");
        std::fs::write(&main, "st = {}\nfunction on_timer() end").unwrap();
        let (tx, mut rx) = mpsc::channel::<()>(8);
        let token = CancellationToken::new();
        spawn_watcher(dir.clone(), tx, token.clone());

        // ① 存活证明：重复写入直至收到通知（容忍注册竞争——慢机上 spawn 后立即
        // 写可能早于 inotify 注册完成，单次写会漏；收到通知即证明 watcher 在监听
        // 且写事件被放行，后续"读静默"才不可能是没观测到）。
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let alive = loop {
            std::fs::write(&main, "st = {}\nfunction on_timer() end\n-- alive").unwrap();
            match tokio::time::timeout(Duration::from_secs(2), rx.recv()).await {
                Ok(Some(())) => break true,
                Ok(None) => break false, // channel 关闭：watcher task 已退出
                Err(_) if tokio::time::Instant::now() < deadline => continue,
                Err(_) => break false,
            }
        };
        assert!(alive, "watcher 存活证明失败：10s 内写入未产生任何通知");

        // ② 读风暴：50 次读（远超真实 load_main 频率）→ 不得有通知。此时 watcher
        // 已被 ① 证明在监听——静默只能是过滤生效，不可能是"没看见"。
        for _ in 0..50 {
            let _ = std::fs::read(&main).unwrap();
        }
        let quiet = tokio::time::timeout(Duration::from_millis(1500), rx.recv()).await;
        assert!(
            matches!(quiet, Err(_)),
            "读取事件不得触发 reload（自激风暴回归），实得 {quiet:?}"
        );

        // ③ 读风暴后再写：仍须通知（读取未污染 watcher 状态）。
        std::fs::write(&main, "st = {}\nfunction on_timer() end\n-- v2").unwrap();
        let r = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await;
        token.cancel();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            matches!(r, Ok(Some(()))),
            "写入应触发 reload 通知，实得 {r:?}"
        );
    }
}
