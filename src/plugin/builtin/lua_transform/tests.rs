//! luaTransform 单测：用假 `ctx` + 可控时钟驱动仓库内置脚本，验证判定逻辑。
//!
//! 现覆盖 `scripts/limit-alarm/main.lua`（iot-demo 限值报警）的模拟量延时：短促尖峰
//! 不报、持续越限到点才报、fire 被拒收后下一采样立即重试、布尔故障不受延时影响。
//! 脚本的宿主集成（manifest / 沙箱 / 热加载）由 e2e 与现场实例覆盖，这里只测判定逻辑。

use mlua::Lua;
use std::sync::{Arc, Mutex};

/// 一次 fire/clear 记录：(动作, key, level)。
type Event = (&'static str, String, String);

/// 假宿主：`ctx:now()` 的返回值由测试推进，fire/clear 落入 `events`；
/// `reject_fires` = 接下来这么多次 `fire_event` 返回 false（模拟频控拒收）。
#[derive(Default)]
struct FakeHost {
    now_ms: f64,
    events: Vec<Event>,
    reject_fires: usize,
}

/// 建 VM、装好假 `ctx`、加载真实的 limit-alarm 脚本并跑 `on_init`。
fn limit_alarm_vm() -> (Lua, Arc<Mutex<FakeHost>>) {
    let host = Arc::new(Mutex::new(FakeHost::default()));
    let lua = Lua::new();
    let ctx = lua.create_table().expect("ctx table");

    let h = Arc::clone(&host);
    let fire = lua
        .create_function(
            move |_, (_self, key, level, _msg): (mlua::Value, String, String, mlua::Value)| {
                let mut host = h.lock().unwrap();
                if host.reject_fires > 0 {
                    host.reject_fires -= 1;
                    return Ok(false);
                }
                host.events.push(("fire", key, level));
                Ok(true)
            },
        )
        .expect("fire_event");
    let h = Arc::clone(&host);
    let clear = lua
        .create_function(move |_, (_self, key): (mlua::Value, String)| {
            h.lock().unwrap().events.push(("clear", key, String::new()));
            Ok(())
        })
        .expect("clear_event");
    let h = Arc::clone(&host);
    let now = lua
        .create_function(move |_, _self: mlua::Value| Ok(h.lock().unwrap().now_ms))
        .expect("now");

    ctx.set("fire_event", fire).expect("set");
    ctx.set("clear_event", clear).expect("set");
    ctx.set("now", now).expect("set");
    lua.globals().set("ctx", ctx).expect("set ctx");
    lua.load(include_str!("../../../../scripts/limit-alarm/main.lua"))
        .exec()
        .expect("脚本应能加载");
    let on_init: mlua::Function = lua.globals().get("on_init").expect("on_init");
    on_init.call::<()>(()).expect("on_init");
    (lua, host)
}

/// 在 `at_s` 时刻投递一个数值采样点（推进假时钟 → 调 `on_rtdata`）。
fn push(lua: &Lua, host: &Arc<Mutex<FakeHost>>, at_s: f64, devid: &str, tag: &str, v: f64) {
    host.lock().unwrap().now_ms = at_s * 1000.0;
    let f: mlua::Function = lua.globals().get("on_rtdata").expect("on_rtdata");
    f.call::<()>((devid, tag, v)).expect("on_rtdata");
}

/// 在 `at_s` 时刻投递一个布尔采样点。
fn push_bool(lua: &Lua, host: &Arc<Mutex<FakeHost>>, at_s: f64, devid: &str, tag: &str, v: bool) {
    host.lock().unwrap().now_ms = at_s * 1000.0;
    let f: mlua::Function = lua.globals().get("on_rtdata").expect("on_rtdata");
    f.call::<()>((devid, tag, v)).expect("on_rtdata");
}

fn events(host: &Arc<Mutex<FakeHost>>) -> Vec<Event> {
    host.lock().unwrap().events.clone()
}

/// 模拟量延时：短促越限不报；回落到回差带内取消计时，再次越限从零重新计时。
#[test]
fn analog_short_excursion_does_not_fire() {
    let (lua, host) = limit_alarm_vm();
    // dev_pt1.pressure_1：H 1.69 / HH 1.695
    for t in 0..=10 {
        push(&lua, &host, f64::from(t), "dev_pt1", "pressure_1", 1.70);
    }
    assert!(
        events(&host).is_empty(),
        "越限 10s（不足 15s 延时）不得报出"
    );

    push(&lua, &host, 11.0, "dev_pt1", "pressure_1", 1.60); // 回落到回差带内
    for t in 12..=20 {
        push(&lua, &host, f64::from(t), "dev_pt1", "pressure_1", 1.70);
    }
    assert!(
        events(&host).is_empty(),
        "回落后再越限只持续 8s，计时已从 12s 重新开始"
    );
}

/// 模拟量延时：持续越限满 15s 才报（到点前零事件），回落后清除。
#[test]
fn analog_sustained_excursion_fires_after_delay() {
    let (lua, host) = limit_alarm_vm();
    for t in 0..=14 {
        push(&lua, &host, f64::from(t), "dev_pt1", "pressure_1", 1.70);
    }
    assert!(events(&host).is_empty(), "第 14s 仍未满 15s");

    push(&lua, &host, 15.0, "dev_pt1", "pressure_1", 1.70);
    assert_eq!(
        events(&host),
        vec![
            (
                "fire",
                "dev_pt1.pressure_1.hh".to_string(),
                "error".to_string()
            ),
            (
                "fire",
                "dev_pt1.pressure_1.hi".to_string(),
                "warn".to_string()
            ),
        ],
        "满 15s：HH(error) 与 H(warn) 同时报出"
    );

    push(&lua, &host, 16.0, "dev_pt1", "pressure_1", 1.60);
    assert_eq!(
        events(&host)[2..],
        vec![
            ("clear", "dev_pt1.pressure_1.hh".to_string(), String::new()),
            ("clear", "dev_pt1.pressure_1.hi".to_string(), String::new()),
        ],
        "回落到回差带内即清除"
    );
}

/// 布尔故障不走延时：立即报、立即清。
#[test]
fn boolean_fault_is_immediate() {
    let (lua, host) = limit_alarm_vm();
    push_bool(&lua, &host, 0.0, "dev_pt1", "fault", true);
    assert_eq!(
        events(&host),
        vec![(
            "fire",
            "dev_pt1.fault.alarm".to_string(),
            "error".to_string()
        )]
    );
    push_bool(&lua, &host, 1.0, "dev_pt1", "fault", false);
    assert_eq!(
        events(&host)[1..],
        vec![("clear", "dev_pt1.fault.alarm".to_string(), String::new())]
    );
}

/// fire 被频控拒收（`ctx:fire_event` 返回 false）后：下一个越限采样立即重试，
/// 不得再等满一个延时（到点即报的条件已满足，拒收不是脚本的问题）。
#[test]
fn analog_fire_retries_on_next_sample_after_reject() {
    let (lua, host) = limit_alarm_vm();
    for t in 0..=14 {
        push(&lua, &host, f64::from(t), "dev_pt1", "pressure_1", 1.70);
    }
    host.lock().unwrap().reject_fires = 2; // 第 15s 的两次 fire（hh、hi）都被拒收
    push(&lua, &host, 15.0, "dev_pt1", "pressure_1", 1.70);
    assert!(events(&host).is_empty(), "被拒收：无事件");

    push(&lua, &host, 16.0, "dev_pt1", "pressure_1", 1.70);
    assert_eq!(
        events(&host),
        vec![
            (
                "fire",
                "dev_pt1.pressure_1.hh".to_string(),
                "error".to_string()
            ),
            (
                "fire",
                "dev_pt1.pressure_1.hi".to_string(),
                "warn".to_string()
            ),
        ],
        "下一采样点立即重试并报出，不重新计时"
    );
}
