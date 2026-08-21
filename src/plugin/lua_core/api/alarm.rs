//! 告警护栏状态 + `ctx:fire_event`/`ctx:clear_event` 注册（Q12 告警最小闭环，
//! spec `2026-08-21-alarm-minimal-loop-design.md` §2/§3；D 阶段 T4）。
//!
//! C6 跨角色能力（§5.2 C6 行 ✓✓✓✓）：collector/transform/north **全注入**，无
//! 角色门——判定场景可能在任一角色（采集侧判原始点 / transform 侧判派生点 /
//! north 侧判平台回执）。
//!
//! 护栏（spec §3，防告警风暴）：
//! - 每实例频控 token bucket 20/s（照 C5 http 的 R2 频控模式，**独立额度**不与
//!   http 共享）：超限返 `false`（非错误——风暴不是脚本 bug，静默退避语义）+
//!   计数落 stat `alarm_rate_limited`（壳层 flush）；
//! - 同 key 1s 抖动：窗口内重复 fire 仍 `count++`（风暴可见性）但**不重发 SSE**
//!   （前端不被同键闪屏）；clear 顺带清抖动表条目（清除后再触发 = 新事件）。

use crate::plugin::lua_core::ratelimit::RateLimiter;
use mlua::{Lua, LuaString, Value};
use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

/// 同 key 抖动窗口（spec §3）：窗口内重复 fire 不重发 SSE（count 仍 ++）。
pub(crate) const ALARM_DEDUPE_WINDOW: std::time::Duration = std::time::Duration::from_secs(1);

/// 告警护栏状态（per-instance，spec §3）：频控 + 抖动表 + 频控计数。
///
/// 经 [`super::ApiCtx`] 的 `alarm` 字段注入（三壳各建一份；reload 复用 ac，
/// 计数跨 VM 替换不丢——与 dropped 同语义）。壳层 flush 读 [`AlarmState::rate_limited_count`]
/// 落 stat。
pub(crate) struct AlarmState {
    /// fire 频控（20/s，限值查能力卡单一事实源；与 http 出站频控独立额度）。
    rl: RateLimiter,
    /// 频控拒绝计数（stat `alarm_rate_limited`）。
    rate_limited: AtomicU64,
    /// 同 key 最近一次 publish 时刻（1s 窗口内命中不重发；Mutex 瞬时返回不跨 await）。
    last_publish: Mutex<HashMap<String, std::time::Instant>>,
}

impl AlarmState {
    /// 新建：`rate_per_sec` 次/s 频控（测试可注入小容量；生产走 [`Default`]）。
    pub(crate) fn new(rate_per_sec: u32) -> Self {
        Self {
            rl: RateLimiter::new(rate_per_sec),
            rate_limited: AtomicU64::new(0),
            last_publish: Mutex::new(HashMap::new()),
        }
    }

    /// 频控检查：放行 true；超限 false 且 `rate_limited` 计数 +1。
    pub(crate) fn check(&self) -> bool {
        if self.rl.check() {
            true
        } else {
            self.rate_limited
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            false
        }
    }

    /// 抖动判定（spec §3）：key 在 1s 窗口内已 publish 过 → true（本次不重发）；
    /// 首发或窗口外 → false 并记当前时刻。顺带 retain 清窗口外条目（防 map 无界
    /// 膨胀——照 `record_self_writes` 模式，低频场景全量 O(n) 扫描即可）。
    pub(crate) fn dedup_hit(&self, key: &str) -> bool {
        let Ok(mut m) = self.last_publish.lock() else {
            return false; // 锁中毒（无 panic 路径不可达）——宁可重发不丢告警帧
        };
        let now = std::time::Instant::now();
        m.retain(|_, t| now.duration_since(*t) < ALARM_DEDUPE_WINDOW);
        if m.contains_key(key) {
            true
        } else {
            m.insert(key.to_string(), now);
            false
        }
    }

    /// 清抖动表条目（clear 路径）：清除后再触发 = 新事件，正常发 SSE——否则
    /// clear→fire 落在旧窗口内会静默吞掉重触发帧（前端面板缺条目）。
    pub(crate) fn clear_key(&self, key: &str) {
        if let Ok(mut m) = self.last_publish.lock() {
            m.remove(key);
        }
    }

    /// 频控拒绝计数快照（壳层 flush 落 stat `alarm_rate_limited`）。
    pub(crate) fn rate_limited_count(&self) -> u64 {
        self.rate_limited.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// 告警频控限值（次/秒/实例，spec §3 固定 20）：查能力卡 `ctx:fire_event` 的
/// `rate_per_sec`——单一事实源（照 http.rs 的 `http_rate_per_sec` 模式）；卡缺失
/// 或 0 时回落保守默认 20。
fn alarm_rate_per_sec() -> u32 {
    crate::plugin::lua_core::capability::CAPABILITIES
        .iter()
        .find(|c| c.name == "ctx:fire_event")
        .map(|c| c.rate_per_sec)
        .filter(|&r| r > 0)
        .unwrap_or(20)
}

impl Default for AlarmState {
    fn default() -> Self {
        Self::new(alarm_rate_per_sec())
    }
}

/// per-instance 告警护栏句柄（ApiCtx 字段形态；照 `RuntimeDevs`/`SelfWrites` 先例）。
pub(crate) type AlarmStats = Arc<AlarmState>;

/// key 入口校验（spec §2）：非空、≤128 字符（fire 与 clear 共用）。
fn validate_key(key: &str) -> Result<(), String> {
    if key.is_empty() {
        Err("key 不能为空".to_string())
    } else if key.chars().count() > 128 {
        Err("key 超长（≤128 字符）".to_string())
    } else {
        Ok(())
    }
}

/// level 入口校验（spec §2）：三档字符串（SSE JSON 直读，前端无映射表）。
fn validate_level(level: &str) -> Result<(), String> {
    if matches!(level, "info" | "warn" | "error") {
        Ok(())
    } else {
        Err(format!("level 非法：{level}（应为 info/warn/error）"))
    }
}

/// message 入口校验（spec §2）：≤512 字符（防告警文案洪水）。
fn validate_message(message: &str) -> Result<(), String> {
    if message.chars().count() > 512 {
        Err("message 超长（≤512 字符）".to_string())
    } else {
        Ok(())
    }
}

/// 注册 C6 告警双函数到 `ctx` 表（`register` 尾段调用，三角色全注入无门）。
///
/// - `ctx:fire_event(key, level, message) -> true | false | nil,err`：
///   true=已上报；false=被频控（spec §2/§3）；nil,err=参数违例（可感知，不落库）。
///   路径：校验 → 频控（超限即拒不写清单）→ lux 覆盖写 count++（[`crate::db::alarms::fire`]）
///   → 同 key 1s 抖动决定是否 publish Fire 帧 → 审计（key 级，不落 message 全文）。
/// - `ctx:clear_event(key) -> true | false`：频控超限返 false（复审 P1-2——与 fire
///   同 20/s 桶）；放行则幂等恒发 Clear 帧（level/message 沿用条目值，无条目空串
///   ——多客户端视图收敛），HDEL 后清抖动表条目（清除后再触发=新事件）。
///
/// 两者均 async（lux 操作 await）→ `create_async_function`（照 kv_set 桥接模式）；
/// `_self` 首参吸收冒号隐式 self；`source` 一律取 `ac.inst.inst_id`（宿主填，
/// 脚本不可伪造——API 无 source 参数）。
pub(super) fn register(lua: &Lua, ctx: &mlua::Table, ac: &super::ApiCtx<'_>) -> mlua::Result<()> {
    // --- ctx:fire_event(key, level, message) -> bool | nil,err ---
    let lux_fire = ac.ctx_host.lux.clone();
    let event_fire = ac.ctx_host.event_tx.clone();
    let inst_fire = ac.inst.inst_id.as_str().to_string();
    let stats_fire = ac.alarm.clone();
    let fire_fn =
        lua.create_async_function(
            move |lua,
                  (_self, key, level, message): (
                Value,
                LuaString,
                LuaString,
                Option<LuaString>,
            )| {
                let lux = lux_fire.clone();
                let event = event_fire.clone();
                let inst = inst_fire.clone();
                let stats = stats_fire.clone();
                async move {
                    let key = key.to_string_lossy().to_string();
                    let level = level.to_string_lossy().to_string();
                    let message = message
                        .map(|m| m.to_string_lossy().to_string())
                        .unwrap_or_default();
                    // 入口校验（spec §2）：违例 nil,err（可感知）——不触频控不落库。
                    if let Err(e) = validate_key(&key)
                        .and_then(|()| validate_level(&level))
                        .and_then(|()| validate_message(&message))
                    {
                        return Ok((
                            Value::Nil,
                            Value::String(lua.create_string(format!("fire_event: {e}"))?),
                        ));
                    }
                    // 频控先行（spec §3）：超限 false（非错误——风暴不是脚本 bug）且
                    // 不写清单不推 SSE（清单/SSE 不被刷爆）。
                    if !stats.check() {
                        return Ok((Value::Boolean(false), Value::Nil));
                    }
                    let ts = crate::event_bus::now_iso8601();
                    match crate::db::alarms::fire(&lux, &key, &level, &inst, &message, &ts).await {
                        Ok(_count) => {
                            // 审计 key 级（spec §2：不落 message 全文防日志洪水）。
                            tracing::info!("[alarm] fire key={key} level={level} by={inst}");
                            // 同 key 1s 抖动（spec §3）：count 已 ++（风暴可见性），
                            // 窗口内不重发 SSE（前端不被同键闪屏）。
                            if !stats.dedup_hit(&key) {
                                event.publish(crate::event_bus::EventPayload::Alarm {
                                    action: crate::event_bus::AlarmAction::Fire,
                                    key,
                                    level,
                                    source: inst,
                                    message,
                                    ts,
                                });
                            }
                            Ok((Value::Boolean(true), Value::Nil))
                        }
                        Err(e) => Err(mlua::Error::RuntimeError(format!("fire_event: {e}"))),
                    }
                }
            },
        )?;
    ctx.set("fire_event", fire_fn)?;

    // --- ctx:clear_event(key) -> bool ---
    let lux_clear = ac.ctx_host.lux.clone();
    let event_clear = ac.ctx_host.event_tx.clone();
    let inst_clear = ac.inst.inst_id.as_str().to_string();
    let stats_clear = ac.alarm.clone();
    let clear_fn = lua.create_async_function(move |lua, (_self, key): (Value, LuaString)| {
        let lux = lux_clear.clone();
        let event = event_clear.clone();
        let inst = inst_clear.clone();
        let stats = stats_clear.clone();
        async move {
            let key = key.to_string_lossy().to_string();
            if let Err(e) = validate_key(&key) {
                return Ok((
                    Value::Nil,
                    Value::String(lua.create_string(format!("clear_event: {e}"))?),
                ));
            }
            // 复审 P1-2：clear 与 fire 同频控（此前仅 fire 有护栏——脚本在回调内
            // 循环 clear 可刷 SSE 帧/审计日志/lux ops 无上限）。超限返 false、
            // 不 HDEL 不发帧（与 fire 超限同形态），rate_limited 计数共用。
            if !stats.check() {
                return Ok((Value::Boolean(false), Value::Nil));
            }
            // 清除前取条目（spec §1.1：clear 沿用清除时条目的 level/message；
            // 无条目空串——value 损坏同样走空串路径，见 db::alarms::get 文档）。
            let (level, message) = match crate::db::alarms::get(&lux, &key).await {
                Ok(Some(item)) => (item.level, item.message),
                Ok(None) => (String::new(), String::new()),
                Err(e) => return Err(mlua::Error::RuntimeError(format!("clear_event: {e}"))),
            };
            crate::db::alarms::clear(&lux, &key)
                .await
                .map_err(|e| mlua::Error::RuntimeError(format!("clear_event: {e}")))?;
            // 恒发 Clear 帧（spec §2 幂等恒发：多客户端视图收敛一致）+ 清抖动
            // 表（清除后再触发 = 新事件，正常发 Fire 而非被旧窗口吞）。
            stats.clear_key(&key);
            let ts = crate::event_bus::now_iso8601();
            tracing::info!("[alarm] clear key={key} by={inst}");
            event.publish(crate::event_bus::EventPayload::Alarm {
                action: crate::event_bus::AlarmAction::Clear,
                key,
                level,
                source: inst,
                message,
                ts,
            });
            Ok((Value::Boolean(true), Value::Nil))
        }
    })?;
    ctx.set("clear_event", clear_fn)?;
    Ok(())
}
