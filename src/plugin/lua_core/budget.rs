//! 回调预算与 stat 快照的公共层（Q3 护栏，复审 P2-1 下沉）。
//!
//! 三薄壳（luaClient/luaNorth/luaTransform）此前各持一份逐字相同的 `with_budget`
//! 与缺省值——预算语义是护栏（§4.5-1），漂移成本高（改消息/钳制规则要同步三处），
//! 收敛到 lua_core 单一事实源；三壳 parse 的缺省常量同此。

/// 单回调时长预算包装：每次调用统一包 `tokio::time::timeout(callback_timeout_ms)`；
/// Elapsed → `Err(RuntimeError("callback timeout: {kind} 超预算 …"))`——消息含回调名
/// 与预算，供 warn 日志与 tick_fail 记账。脚本自身错误原样透传（严格区分）。
pub(crate) async fn with_budget<T>(
    kind: &str,
    budget_ms: u64,
    f: impl std::future::Future<Output = T>,
) -> Result<T, mlua::Error> {
    match tokio::time::timeout(std::time::Duration::from_millis(budget_ms), f).await {
        Ok(v) => Ok(v),
        Err(_) => Err(mlua::Error::RuntimeError(format!(
            "callback timeout: {kind} 超预算 {budget_ms}ms，跳过本轮"
        ))),
    }
}

/// 运行时设备集快照 → tracker extra（BTreeSet 有序 → Vec 稳定，前端 diff 不抖动；
/// 实例的上报设备集经 set_value 记入）。
pub(crate) fn sync_runtime_devs(
    tracker: &mut crate::plugin::stat_tracker::StatTracker,
    devs: &super::api::RuntimeDevs,
) {
    let snapshot: Vec<String> = devs
        .lock()
        .map(|s| s.iter().cloned().collect())
        .unwrap_or_default();
    tracker.extra_insert("runtime_devices", serde_json::json!(snapshot));
}

/// 三壳共用的配置缺省（Q3 护栏/沙箱参数——单一事实源，防三处漂移）。
pub(crate) const DEFAULT_CALLBACK_TIMEOUT_MS: u64 = 10_000;
/// 连续失败降级阈值（Q3「连续超限走 fail_threshold → degraded」）。
pub(crate) const DEFAULT_FAIL_THRESHOLD: u32 = 10;
/// VM 内存限额 MB（沙箱护栏缺省）。
pub(crate) const DEFAULT_MEM_LIMIT_MB: usize = 32;
/// 指令 hook 阈值（沙箱护栏缺省，1M ≈ 单次纯计算 10-30ms）。
pub(crate) const DEFAULT_HOOK_INSTR_LIMIT: u32 = 1_000_000;
