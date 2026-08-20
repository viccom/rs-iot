//! mqtt-north 回调契约薄包装 + mqtt task（Q5 定案全项，阶段 B T7）。
//!
//! 架构（照 spike-2 的 EventLoop 独占 pump 模型，[`crate::plugin::lua_core::io_engine::spike2_mqtt`]）：
//! - 主循环外 spawn **mqtt task**：独占 `EventLoop::poll()`，只做网络 IO——命令通道
//!   （[`MqttCmd`]）收连接/发布/订阅，事件通道（[`MqttEvt`]）回 ConnAck/断连/下行/
//!   发布计数；
//! - auth/will/up/down/on_connack/on_disconnect 回调**全在主循环侧调**（VM 只有一份，
//!   回调串行化更稳）；up() 返回的发布列表交 mqtt task 逐条投递（fire-and-forget，
//!   计数经 [`MqttEvt::PubResult`] 回传）；
//! - **重连必重调 auth()**（Q5：token 型认证刷新）：断连后 drop 旧 EventLoop，主循环
//!   重新走连接序列（will → auth → password 解析 → 新 MqttOptions → `MqttCmd::Connect`
//!   重建 EventLoop），凭证刷新才生效。退避 = `period` 秒（与 lua_client 重连哲学一致）。
//!
//! 连接序列（Q5 定案）：`will()`（可选）→ `auth(cfg)`（必需，manifest 已锁）→ 返回
//! `{client_id?, username?, password?, keepalive?}` 覆盖 NorthCfg 对应项（cfg 项为
//! 缺省，auth 返回优先）→ password 的 `secret:` 引用经 [`resolve_password_opt`] 解析
//! （auth 返回与 NorthCfg.password 都支持）→ 组 [`rumqttc::MqttOptions`] 连接。

use std::collections::BTreeMap;

use crate::types::Sample;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::config::NorthCfg;

// ===== mqtt task：通道协议（主循环 ↔ 网络 IO task）=====

/// 一条待发布消息（up() 返回数组的解析产物；qos 缺省 AtMostOnce）。
#[derive(Debug)]
pub(crate) struct PubItem {
    pub(crate) topic: String,
    pub(crate) payload: Vec<u8>,
    pub(crate) qos: rumqttc::QoS,
}

/// 主循环 → mqtt task 命令。
pub(crate) enum MqttCmd {
    /// 连接/重连：drop 旧连接（client+eventloop），用新 MqttOptions（auth 已重调）
    /// 重建——Q5 凭证刷新经此生效（rumqttc 自带重连不会换 MqttOptions，故必须重建）。
    /// Box 压 variant 大小差（clippy large_enum_variant）。
    Connect(Box<rumqttc::MqttOptions>),
    /// 批量发布（fire-and-forget；结果计数经 [`MqttEvt::PubResult`] 回传）。
    Publish(Vec<PubItem>),
    /// 订阅（ConnAck 后主循环发；空列表不订阅）。
    Subscribe(Vec<String>),
}

/// mqtt task → 主循环 事件。
pub(crate) enum MqttEvt {
    /// ConnAck（连接成功；主循环后接 on_connack + subscribe）。
    ConnAck,
    /// 断连/连接失败（pump 已停转等重连指令；主循环走重连序列）。
    Disconnected(String),
    /// 下行消息（订阅 topic 上的 Publish；T7 全量路由 down()——命令 topic 是 T8 概念）。
    Down { topic: String, payload: Vec<u8> },
    /// 批量发布结果计数（publish Ok 只代表入队/投递接受，见 rumqttc 语义）。
    PubResult { ok: usize, fail: usize },
}

/// flume 请求通道容量（AsyncClient::new 第二参）。pump 独立排空（见下）后该值只是
/// publish 侧的流控背压水位，不再是死锁面（复审阻塞-1：单任务时代 poll 与 publish
/// 互相卡死，>cap 条必挂死）。64 给大批量留余量。
const REQUEST_CAP: usize = 64;

/// 订阅失败累计（stat extra `subscribe_fail` 的数据源；复审 P2-5：下行通道静默失效
/// 可见化——重试 = 下个 ConnAck，主循环每次 ConnAck 后重发订阅清单）。
pub(crate) static SUBSCRIBE_FAIL: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// spawn mqtt task（网络 IO 独占；不触 VM/回调——回调全在主循环侧）。
///
/// **双任务形态（复审阻塞-1 重构，回归 spike-2 核心决策）**：
/// - **pump 任务**：独占 `eventloop.poll()`——不进任何 select!（规避 poll 取消安全
///   未知问题）、绝不 await publish。事件路由：ConnAck/Disconnected 为状态迁移事件
///   用 `send().await` **必达**（复审 P2-2：ConnAck try_send 满丢会让 connected
///   永假、发布停摆直至下次断连）；Down 单条非关键 try_send。
/// - **handler 任务**（返回的 JoinHandle）：持 client 处理命令。publish/subscribe 在
///   本任务 await，pump 并发排空通道——两者互不阻塞（单任务时代的死锁面消除）；
///   批操作整体包 token select（pump 停转通道满时批内 await 仍可被取消）。
/// - Connect：abort 旧 pump + drop 旧连接（在飞排队请求随之丢弃——fire-and-forget
///   语义），用新 MqttOptions 重建（Q5 凭证刷新）并 respawn pump。pump 在
///   Disconnect/poll Err 后自行退出（不依赖 rumqttc 自带重连——MqttOptions 已定死，
///   重连必须走主循环 auth 序列）；pump 退出后 publish 因通道无接收端立即 Err 计 fail。
pub(crate) fn spawn_mqtt_task(
    mut cmd_rx: mpsc::Receiver<MqttCmd>,
    evt_tx: mpsc::Sender<MqttEvt>,
    token: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut client: Option<rumqttc::AsyncClient> = None;
        let mut pump: Option<tokio::task::JoinHandle<()>> = None;
        loop {
            tokio::select! {
                biased;
                _ = token.cancelled() => break,
                cmd = cmd_rx.recv() => match cmd {
                    None => break,
                    // 重建连接：abort 旧 pump + drop 旧 client（在飞请求丢弃），
                    // Q5 凭证刷新经新 MqttOptions 生效。
                    Some(MqttCmd::Connect(opts)) => {
                        if let Some(p) = pump.take() {
                            p.abort();
                        }
                        let (c, e) = rumqttc::AsyncClient::new(*opts, REQUEST_CAP);
                        client = Some(c);
                        let evt = evt_tx.clone();
                        pump = Some(tokio::spawn(async move {
                            let mut e = e;
                            loop {
                                match e.poll().await {
                                    Ok(rumqttc::Event::Incoming(rumqttc::Packet::ConnAck(_))) => {
                                        tracing::info!("[luaNorth] MQTT ConnAck（已连接）");
                                        // 状态迁移事件必达（阻塞 send = 对主循环的正确背压）
                                        let _ = evt.send(MqttEvt::ConnAck).await;
                                    }
                                    Ok(rumqttc::Event::Incoming(rumqttc::Packet::Publish(p))) => {
                                        // T7 down() 全量路由（命令 topic 是 T8 兼容脚本概念）。
                                        let _ = evt.try_send(MqttEvt::Down {
                                            topic: p.topic.clone(),
                                            payload: p.payload.to_vec(),
                                        });
                                    }
                                    Ok(rumqttc::Event::Incoming(rumqttc::Packet::Disconnect)) => {
                                        let _ = evt
                                            .send(MqttEvt::Disconnected("broker 发送 Disconnect".into()))
                                            .await;
                                        break; // 停转：重连走主循环 Connect（auth 序列）
                                    }
                                    Ok(_) => {}
                                    Err(e) => {
                                        let _ = evt.send(MqttEvt::Disconnected(e.to_string())).await;
                                        break;
                                    }
                                }
                            }
                        }));
                    }
                    // 逐条投递（fire-and-forget）：publish Ok = 入队/投递接受（rumqttc
                    // 未连接时排队返回 Ok，spike-2 实测）；Err 记 fail 不中断。pump 并发
                    // 排空，通道满只是背压等待（不再死锁）；整批包 token（可取消）。
                    Some(MqttCmd::Publish(items)) => {
                        let mut ok = 0usize;
                        let mut fail = 0usize;
                        if let Some(c) = &client {
                            let batch = async {
                                for it in items {
                                    match c
                                        .publish(it.topic.clone(), it.qos, false, it.payload.clone())
                                        .await
                                    {
                                        Ok(()) => ok += 1,
                                        Err(e) => {
                                            fail += 1;
                                            tracing::debug!("[luaNorth] publish {} 失败：{e}", it.topic);
                                        }
                                    }
                                }
                            };
                            tokio::select! {
                                _ = token.cancelled() => break,
                                _ = batch => {}
                            }
                        } else {
                            fail = items.len();
                        }
                        let _ = evt_tx.try_send(MqttEvt::PubResult { ok, fail });
                    }
                    Some(MqttCmd::Subscribe(topics)) => {
                        if let Some(c) = &client {
                            let batch = async {
                                for t in topics {
                                    if let Err(e) =
                                        c.subscribe(t.clone(), rumqttc::QoS::AtMostOnce).await
                                    {
                                        SUBSCRIBE_FAIL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                        tracing::warn!("[luaNorth] subscribe {t} 失败（累计 {}）：{e}",
                                            SUBSCRIBE_FAIL.load(std::sync::atomic::Ordering::Relaxed));
                                    }
                                }
                            };
                            tokio::select! {
                                _ = token.cancelled() => break,
                                _ = batch => {}
                            }
                        }
                    }
                },
            }
        }
        if let Some(p) = pump.take() {
            p.abort();
        }
    })
}

// ===== 连接序列（主循环侧：回调 → MqttOptions → Connect 命令）=====

/// 连接序列失败（`fatal` = 重试也不会好，宿主退出实例；否则等一个重连周期重试）。
pub(crate) struct ConnectErr {
    pub(crate) fatal: bool,
    pub(crate) msg: String,
}

/// 连接序列：`will()` → `auth(cfg)` → password 解析（`secret:` 引用）→ 组 MqttOptions
/// → 发 `MqttCmd::Connect`。**每次（重）连接都完整重走**——Q5 重连必重调 auth。
///
/// 失败分类（照「大声失败」惯例）：
/// - Fatal：password secret 引用非法 / lux 读取错误 / mqtt task 通道关闭——启动即 warn 退出；
/// - Retry：auth/will 回调失败或超预算、auth 返回值类型不符、secret 引用未写（按认证
///   失败处理，不静默改用无密码连接）——等 `period` 秒重试（管理员补写 secret 可救活）。
pub(crate) async fn start_connect(
    lua: &mlua::Lua,
    cfg: &NorthCfg,
    lux: &lux::EmbeddedClient,
    hwid: &str,
    inst_id: &str,
    cmd_tx: &mpsc::Sender<MqttCmd>,
) -> Result<(), ConnectErr> {
    let retry = |msg: String| ConnectErr { fatal: false, msg };
    // 1. will()（可选回调；globals 无则跳过，返回 nil → 无 LWT）。
    let will_v = with_budget("will", cfg.callback_timeout_ms, call_optional(lua, "will"))
        .await
        .map_err(|e| retry(format!("will() {e}")))?
        .map_err(|e| retry(format!("will() {e}")))?;
    let will = will_to_lastwill(will_v).map_err(|e| retry(format!("will() 返回值 {e}")))?;
    // 2. auth(cfg)（必需回调，manifest @role mqtt-north 已锁；幂等要求见设计 §5.1）。
    if lua.globals().get::<mlua::Function>("auth").is_err() {
        return Err(retry(
            "脚本未定义 auth()（@role mqtt-north 契约必需）".into(),
        ));
    }
    let cfg_json = cfg.to_script_cfg(hwid, inst_id);
    let auth_v = with_budget("auth", cfg.callback_timeout_ms, call_auth(lua, &cfg_json))
        .await
        .map_err(|e| retry(format!("auth() {e}")))?
        .map_err(|e| retry(format!("auth() {e}")))?;
    let ov = apply_auth_overrides(auth_v).map_err(|e| retry(format!("auth() 返回值 {e}")))?;
    // 3. password 解析（auth 返回优先 / cfg 缺省；`secret:` 引用经 lux 解析）。
    //    解析 Err（ref 非法 / lux 错误）= 配置级错误，重试也不会好 → Fatal。
    let pw_declared = ov.password.clone().or_else(|| cfg.password.clone());
    let resolved = resolve_password_opt(pw_declared.as_deref(), lux)
        .await
        .map_err(|e| ConnectErr {
            fatal: true,
            msg: format!("password 解析失败：{e}"),
        })?;
    // 声明了 password 但 secret 引用未写 → 按认证失败处理（Retry：补写 secret
    // 后下一轮重连即救活），不静默改用无密码连接。
    if pw_declared.is_some() && resolved.is_none() {
        return Err(retry(
            "password 的 secret 引用尚未写入 lux（按认证失败处理，不无密码连接）".into(),
        ));
    }
    // 4. 组 MqttOptions → 交 mqtt task 重建 EventLoop（凭证刷新生效点）。
    let opts = build_mqtt_options(cfg, &ov, resolved, will, hwid, inst_id);
    cmd_tx
        .send(MqttCmd::Connect(Box::new(opts)))
        .await
        .map_err(|e| ConnectErr {
            fatal: true,
            msg: format!("mqtt task 通道关闭：{e}"),
        })
}

// ===== 纯函数（单测覆盖）=====

/// auth() 返回的覆盖项（cfg 项为缺省，auth 返回优先）。
#[derive(Default, Debug)]
pub(crate) struct AuthOverrides {
    pub(crate) client_id: Option<String>,
    pub(crate) username: Option<String>,
    pub(crate) password: Option<String>,
    pub(crate) keepalive: Option<u64>,
}

/// table 字段 → Option<String>：nil → None；string → Some；类型不符 → Err。
fn opt_string(t: &mlua::Table, key: &str) -> mlua::Result<Option<String>> {
    match t.get::<mlua::Value>(key)? {
        mlua::Value::Nil => Ok(None),
        mlua::Value::String(s) => Ok(Some(s.to_string_lossy().to_string())),
        other => Err(mlua::Error::RuntimeError(format!(
            "字段 {key} 需 string 或缺省，实 {other:?}"
        ))),
    }
}

/// auth() 返回值 → 覆盖项：nil → 全空（cfg 缺省生效）；table → 逐字段提取（字段类型
/// 不符 → Err，坏声明比静默缺省更危险）；非 table/nil → Err（契约违约）。
pub(crate) fn apply_auth_overrides(v: mlua::Value) -> mlua::Result<AuthOverrides> {
    match v {
        mlua::Value::Nil => Ok(AuthOverrides::default()),
        mlua::Value::Table(t) => {
            let keepalive = match t.get::<mlua::Value>("keepalive")? {
                mlua::Value::Nil => None,
                // MQTT keepalive 上限 18h12m15s（u16 秒）；0 或超限 → Err（坏声明可见）。
                mlua::Value::Integer(i) if (1..=u16::MAX as i64).contains(&i) => Some(i as u64),
                mlua::Value::Number(n) if (1.0..=u16::MAX as f64).contains(&n) => Some(n as u64),
                other => {
                    return Err(mlua::Error::RuntimeError(format!(
                        "字段 keepalive 需 1-65535 秒，实 {other:?}"
                    )))
                }
            };
            Ok(AuthOverrides {
                client_id: opt_string(&t, "client_id")?,
                username: opt_string(&t, "username")?,
                password: opt_string(&t, "password")?,
                keepalive,
            })
        }
        other => Err(mlua::Error::RuntimeError(format!(
            "auth() 返回需 table 或 nil，实 {other:?}"
        ))),
    }
}

/// Lua qos 值 → rumqttc QoS：缺省/非法/nil → AtMostOnce(0)；1 → AtLeastOnce；
/// 2 → ExactlyOnce。
fn qos_from_lua(v: mlua::Value) -> rumqttc::QoS {
    let n = match v {
        mlua::Value::Integer(i) => i as f64,
        mlua::Value::Number(f) => f,
        _ => return rumqttc::QoS::AtMostOnce,
    };
    match n as u64 {
        1 => rumqttc::QoS::AtLeastOnce,
        2 => rumqttc::QoS::ExactlyOnce,
        _ => rumqttc::QoS::AtMostOnce,
    }
}

/// will() 返回值 → LastWill：nil → None（无 LWT）；`{topic, payload, qos?, retain?}`
/// → Some（qos 缺省 0、非法值降 0；retain 缺省 false）。非 table/nil → Err。
pub(crate) fn will_to_lastwill(v: mlua::Value) -> mlua::Result<Option<rumqttc::LastWill>> {
    match v {
        mlua::Value::Nil => Ok(None),
        mlua::Value::Table(t) => {
            let topic = opt_string(&t, "topic")?
                .ok_or_else(|| mlua::Error::RuntimeError("will() 返回缺 topic（string）".into()))?;
            let payload = match t.get::<mlua::Value>("payload")? {
                mlua::Value::Nil => {
                    return Err(mlua::Error::RuntimeError(
                        "will() 返回缺 payload（string）".into(),
                    ))
                }
                mlua::Value::String(s) => s.as_bytes().to_vec(),
                other => {
                    return Err(mlua::Error::RuntimeError(format!(
                        "will() 的 payload 需 string，实 {other:?}"
                    )))
                }
            };
            let qos = qos_from_lua(t.get::<mlua::Value>("qos")?);
            let retain = match t.get::<mlua::Value>("retain")? {
                mlua::Value::Nil => false,
                mlua::Value::Boolean(b) => b,
                other => {
                    return Err(mlua::Error::RuntimeError(format!(
                        "will() 的 retain 需 boolean，实 {other:?}"
                    )))
                }
            };
            Ok(Some(rumqttc::LastWill::new(topic, payload, qos, retain)))
        }
        other => Err(mlua::Error::RuntimeError(format!(
            "will() 返回需 table 或 nil，实 {other:?}"
        ))),
    }
}

/// 组 MqttOptions：auth 覆盖优先（client_id/username/password/keepalive），cfg 缺省
/// （client_id 缺省 `hwid-<inst_id>` 已物化、keepalive 缺省 30s）；credentials 仅在
/// username+password 齐备时设置（照 mqtt_pub 模式）。
pub(crate) fn build_mqtt_options(
    cfg: &NorthCfg,
    ov: &AuthOverrides,
    password: Option<String>,
    will: Option<rumqttc::LastWill>,
    hwid: &str,
    inst_id: &str,
) -> rumqttc::MqttOptions {
    let client_id = ov
        .client_id
        .clone()
        .unwrap_or_else(|| cfg.client_id_or_default(hwid, inst_id));
    let username = ov.username.clone().or_else(|| cfg.username.clone());
    let keepalive = ov.keepalive.unwrap_or(30);
    let mut opts = rumqttc::MqttOptions::new(client_id, cfg.host.clone(), cfg.port);
    opts.set_keep_alive(std::time::Duration::from_secs(keepalive));
    opts.set_clean_session(true);
    match (&username, &password) {
        (Some(u), Some(p)) => {
            opts.set_credentials(u, p);
        }
        // 复审 P2-4：单项声明被 MQTT 协议约束丢弃（set_credentials 无法表达
        // username-only/password-only）——fail 可见而非静默匿名连接后无限重连
        // 「not authorized」查因无门。
        (Some(_), None) => tracing::warn!(
            "[luaNorth] 仅声明 username（password 缺失）——MQTT 协议要求成对，             本次按匿名连接（如需认证请补 password 或 auth() 返回两项）"
        ),
        (None, Some(_)) => tracing::warn!(
            "[luaNorth] 仅声明 password（username 缺失）——MQTT 协议要求成对，             本次按匿名连接（token 型 broker 若支持无用户名密码请经 broker 侧适配）"
        ),
        (None, None) => {}
    }
    if let Some(w) = will {
        opts.set_last_will(w);
    }
    opts
}

/// 解析最终 password（auth 返回优先 / cfg 缺省，None → None 无密码）：
/// 字面值原样；`secret:<ref>` 引用与 `ctx:secret_get` 同规则校验后读 lux secrets 表
/// （读到 → 明文；未写 → `Ok(None)` 由调用方按认证失败处理，不静默无密码连接；
/// lux 错误 / ref 非法 → Err）。注意：north 的 password 引用不受实例 `secrets`
/// 授权清单约束（清单管脚本侧 secret_get；password 是宿主侧连接配置）。
pub(crate) async fn resolve_password_opt(
    pw: Option<&str>,
    lux: &lux::EmbeddedClient,
) -> Result<Option<String>, String> {
    let Some(p) = pw else {
        return Ok(None);
    };
    let Some(r) = p.strip_prefix(crate::db::secrets::REF_PREFIX) else {
        return Ok(Some(p.to_string()));
    };
    let full = format!("{}{r}", crate::db::secrets::REF_PREFIX);
    crate::db::secrets::validate_ref(&full).map_err(|e| e.to_string())?;
    crate::db::secrets::get(lux, &full)
        .await
        .map_err(|e| format!("password secret 读取失败：{e}"))
}

// ===== flush（BatchTick：lux pull 收拢 + rx 缓冲合并 → up() → 逐条投递）=====

/// flush 一轮的记账出口。
pub(crate) enum FlushOutcome {
    /// 无数据（lux 无 rtdata 且 rx 缓冲空）——不调 up、不记账（防空返回 warn 刷屏）。
    Skipped,
    /// 调了 up()：发布项已交 mqtt task；`parse_fail` = up 返回里解析失败的条数
    /// （计 publish_fail，不中断其余条目）。
    Up { parse_fail: usize },
}

/// 单设备批量条目：lux pull（rtdata 快照）与 rx 缓冲样本的统一形状。
#[derive(Debug)]
pub(crate) struct BatchEntry {
    pub(crate) devid: String,
    /// tag → 值（lux cell 的 value 原样 JSON；tag 已剥 `<devid>.` 前缀）。
    pub(crate) values: BTreeMap<String, serde_json::Value>,
    /// 设备最新 ts（lux cell 的 ts 取最大；rx 样本用 sample.timestamp）。
    pub(crate) ts: i64,
}

/// lux pull 收拢：`KEYS rtdata:*` → 逐设备 `HGETALL` → 批量条目（参考 MqttPub 的
/// publish_rtdata 遍历方式；north 组的是 batch 数组而非直接发布）。KEYS 失败 → Err
/// （tick_fail 记账）；单设备 HGETALL 失败跳过（per-devid 容错，同 write_batch 语义）。
pub(crate) async fn pull_rtdata_batch(
    lux: &lux::EmbeddedClient,
) -> Result<Vec<BatchEntry>, String> {
    let keys = lux
        .execute_value("KEYS", &["rtdata:*"])
        .await
        .map_err(|e| format!("lux KEYS rtdata:* 失败：{e}"))?;
    let lux::EmbeddedValue::Array(keys) = keys else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for k in keys {
        let key = match &k {
            lux::EmbeddedValue::Bulk(b) => String::from_utf8_lossy(b).into_owned(),
            lux::EmbeddedValue::Simple(s) => s.clone(),
            _ => continue,
        };
        let devid = key.strip_prefix("rtdata:").unwrap_or(&key).to_string();
        let arr = match lux.execute_value("HGETALL", &[&key]).await {
            Ok(lux::EmbeddedValue::Array(a)) => a,
            _ => continue, // per-devid 容错
        };
        // field=<devid>.<tag>（或 <tag>）→ 剥前缀得 tag（同 ctx:get_device 语义）；
        // cell=[ts, localTime, value] → value 原样 + ts 取最大（同 mqtt_pub parse_rtdata_hash）。
        let prefix = format!("{devid}.");
        let mut values = BTreeMap::new();
        let mut ts: i64 = 0;
        let mut i = 0;
        while i + 1 < arr.len() {
            // field/cell 分别取（Bulk/Simple 任意组合，同 mqtt_pub parse_rtdata_hash）。
            let field = match &arr[i] {
                lux::EmbeddedValue::Bulk(b) => String::from_utf8_lossy(b).into_owned(),
                lux::EmbeddedValue::Simple(s) => s.clone(),
                _ => {
                    i += 2;
                    continue;
                }
            };
            let raw = match &arr[i + 1] {
                lux::EmbeddedValue::Bulk(b) => String::from_utf8_lossy(b).into_owned(),
                lux::EmbeddedValue::Simple(s) => s.clone(),
                _ => {
                    i += 2;
                    continue;
                }
            };
            let tag = field.strip_prefix(&prefix).unwrap_or(&field).to_string();
            match serde_json::from_str::<serde_json::Value>(&raw) {
                Ok(cell) if cell.as_array().is_some_and(|a| a.len() >= 3) => {
                    let cell = cell.as_array().expect("just checked");
                    if let Some(t0) = cell[0].as_i64() {
                        ts = ts.max(t0);
                    }
                    values.insert(tag, cell[2].clone());
                }
                _ => {
                    // 非 [ts,localTime,value] 格式的 cell 原样保留为字符串（兼容旧数据）。
                    values.insert(tag, serde_json::Value::String(raw));
                }
            }
            i += 2;
        }
        if !values.is_empty() {
            out.push(BatchEntry { devid, values, ts });
        }
    }
    Ok(out)
}

/// 组 up(batch) 的 Lua table：lux pull 条目 + rx 缓冲样本 → `{{devid=…, values={tag=v},
/// ts=…}, …}` 数组。
pub(crate) fn batch_to_lua(
    lua: &mlua::Lua,
    entries: &[BatchEntry],
    samples: &[Sample],
) -> mlua::Result<mlua::Table> {
    use mlua::LuaSerdeExt;
    let t = lua.create_table()?;
    let mut idx = 1usize;
    let mut push =
        |devid: &str, values: &BTreeMap<String, serde_json::Value>, ts: i64| -> mlua::Result<()> {
            let item = lua.create_table()?;
            item.set("devid", devid)?;
            item.set("values", lua.to_value(values)?)?;
            item.set("ts", ts)?;
            t.set(idx, item)?;
            idx += 1;
            Ok(())
        };
    for e in entries {
        push(&e.devid, &e.values, e.ts)?;
    }
    for s in samples {
        let values: BTreeMap<String, serde_json::Value> = s
            .values
            .iter()
            .map(|(k, v)| (k.clone(), sample_value_to_json(v)))
            .collect();
        push(&s.devid, &values, s.timestamp)?;
    }
    Ok(t)
}

/// SampleValue → JSON（batch 组装中转；Lua table 经 to_value 由 JSON 桥接）。
fn sample_value_to_json(v: &crate::types::SampleValue) -> serde_json::Value {
    use crate::types::SampleValue;
    match v {
        SampleValue::Int(i) => serde_json::json!(i),
        SampleValue::Float(f) => serde_json::json!(f),
        SampleValue::Bool(b) => serde_json::json!(b),
        SampleValue::Str(s) => serde_json::json!(s),
    }
}

/// up() 返回数组解析：数组元素 `{topic, payload, qos?}`（qos 缺省 0、非法降 0）→
/// [`PubItem`] 列表；topic/payload 非 string 或元素非 table → 该条计 `parse_fail`
/// 跳过不中断；nil / 空数组 → 空 items；非数组 → Err（契约违约）。
pub(crate) fn parse_up_result(v: mlua::Value) -> Result<(Vec<PubItem>, usize), String> {
    match v {
        mlua::Value::Nil => Ok((Vec::new(), 0)),
        mlua::Value::Table(t) => {
            let mut items = Vec::new();
            let mut fail = 0usize;
            for entry in t.sequence_values::<mlua::Value>() {
                // 复审 P3-3：数组空洞（{1=ok, 3=ok} 的 2=nil）与坏条目同语义——
                // 计 parse_fail 跳过，不再静默截断其后全部条目。
                let Ok(entry) = entry else {
                    fail += 1;
                    continue;
                };
                let mlua::Value::Table(e) = entry else {
                    fail += 1;
                    continue;
                };
                let topic = match e.get::<mlua::Value>("topic") {
                    Ok(mlua::Value::String(s)) => s.to_string_lossy().to_string(),
                    _ => {
                        fail += 1;
                        continue;
                    }
                };
                let payload = match e.get::<mlua::Value>("payload") {
                    Ok(mlua::Value::String(s)) => s.as_bytes().to_vec(),
                    _ => {
                        fail += 1;
                        continue;
                    }
                };
                let qos = qos_from_lua(e.get::<mlua::Value>("qos").unwrap_or(mlua::Value::Nil));
                items.push(PubItem {
                    topic,
                    payload,
                    qos,
                });
            }
            Ok((items, fail))
        }
        other => Err(format!("up() 返回需数组（table）或 nil，实 {other:?}")),
    }
}

/// flush 一轮：lux pull + rx 缓冲合并 → `up(batch)`（with_budget 预算）→ 解析返回
/// 数组 → 发 `MqttCmd::Publish`。up 超预算/失败 → Err（调用方 tick_fail，跳过本轮）。
/// rx 缓冲 flush 后清空（`std::mem::take`）。
pub(crate) async fn flush_batch(
    lua: &mlua::Lua,
    lux: &lux::EmbeddedClient,
    cfg: &NorthCfg,
    batch_buf: &mut Vec<Sample>,
    cmd_tx: &mpsc::Sender<MqttCmd>,
) -> Result<FlushOutcome, String> {
    let entries = pull_rtdata_batch(lux).await?;
    if entries.is_empty() && batch_buf.is_empty() {
        return Ok(FlushOutcome::Skipped);
    }
    let samples = std::mem::take(batch_buf);
    // 复审 P2-3：take 之后的失败路径（up 超预算/组装/解析/通道）属瞬时性故障——
    // 样本放回缓冲待下轮（与上方 lux pull 失败保留缓冲同语义；此前 Err 即整轮
    // 蒸发且无计数）。flush 期间主循环单任务不回 select，缓冲不会有并发写入。
    let r = flush_inner(lua, cfg, &entries, &samples, cmd_tx).await;
    if r.is_err() {
        *batch_buf = samples;
    }
    r
}

/// [`flush_batch`] 的 take 后主体（samples 只读；Err 语义见调用方回填）。
async fn flush_inner(
    lua: &mlua::Lua,
    cfg: &NorthCfg,
    entries: &[BatchEntry],
    samples: &[Sample],
    cmd_tx: &mpsc::Sender<MqttCmd>,
) -> Result<FlushOutcome, String> {
    let batch_t = batch_to_lua(lua, entries, samples).map_err(|e| format!("batch 组装 {e}"))?;
    let up_v = with_budget("up", cfg.callback_timeout_ms, call_up(lua, batch_t))
        .await
        .map_err(|e| format!("up() {e}"))?
        .map_err(|e| format!("up() {e}"))?;
    let (items, parse_fail) = parse_up_result(up_v)?;
    if items.is_empty() {
        if parse_fail == 0 {
            tracing::warn!("[luaNorth] up() 返回空（本轮数据未发布）");
        }
        return Ok(FlushOutcome::Up { parse_fail });
    }
    cmd_tx
        .send(MqttCmd::Publish(items))
        .await
        .map_err(|e| format!("publish 通道关闭：{e}"))?;
    Ok(FlushOutcome::Up { parse_fail })
}

// ===== 回调薄包装（主循环调用；可选回调缺失 → 静默跳过）=====

/// 调可选全局回调（无参；未定义 → Ok(Nil)，will 用此形态）。
async fn call_optional(lua: &mlua::Lua, name: &str) -> mlua::Result<mlua::Value> {
    let Ok(f) = lua.globals().get::<mlua::Function>(name) else {
        return Ok(mlua::Value::Nil);
    };
    f.call_async(mlua::Value::Nil).await
}

/// 调 auth(cfg)（必需回调；cfg 为 to_script_cfg 的 JSON 数据转 table）。
async fn call_auth(lua: &mlua::Lua, cfg_json: &serde_json::Value) -> mlua::Result<mlua::Value> {
    use mlua::LuaSerdeExt;
    let f: mlua::Function = lua.globals().get("auth")?;
    let cfg: mlua::Value = lua.to_value(cfg_json)?;
    f.call_async(cfg).await
}

/// 调 up(batch)（必需回调；batch 由 [`batch_to_lua`] 组装）。
async fn call_up(lua: &mlua::Lua, batch: mlua::Table) -> mlua::Result<mlua::Value> {
    let f: mlua::Function = lua.globals().get("up")?;
    f.call_async(mlua::Value::Table(batch)).await
}

/// 调 down(topic, payload)（可选；未定义 → Ok(()) 静默——down 缺失不告警，§5.1 可选）。
pub(crate) async fn call_down(lua: &mlua::Lua, topic: &str, payload: &[u8]) -> mlua::Result<()> {
    let Ok(f) = lua.globals().get::<mlua::Function>("down") else {
        return Ok(());
    };
    let t = lua.create_string(topic)?;
    let p = lua.create_string(payload)?;
    let _: mlua::Value = f.call_async((t, p)).await?;
    Ok(())
}

/// 调 on_connack()（可选；未定义 → Ok(())）。
pub(crate) async fn call_on_connack(lua: &mlua::Lua) -> mlua::Result<()> {
    let _: mlua::Value = call_optional(lua, "on_connack").await?;
    Ok(())
}

/// 调 on_disconnect()（可选；未定义 → Ok(())）。
pub(crate) async fn call_on_disconnect(lua: &mlua::Lua) -> mlua::Result<()> {
    let _: mlua::Value = call_optional(lua, "on_disconnect").await?;
    Ok(())
}

/// 单回调时长预算包装（照 lua_client with_budget 模式，north 本地实现）：超预算 →
/// `Err(RuntimeError("callback timeout: {kind} …"))`，供 warn 日志与 tick_fail 记账；
/// 脚本自身错误原样透传（严格区分）。auth/will/up/down/on_* 全走此包装。
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

#[cfg(test)]
mod tests {
    use super::*;

    // ===== apply_auth_overrides =====

    /// auth 返回全字段 → 逐项提取（nil 值字段视为未覆盖）。
    #[test]
    fn auth_overrides_full_table() {
        let lua = mlua::Lua::new();
        let v: mlua::Value = lua
            .load(r#"return { client_id = "cid-1", username = "u1", password = "p1", keepalive = 60 }"#)
            .eval()
            .unwrap();
        let ov = apply_auth_overrides(v).unwrap();
        assert_eq!(ov.client_id.as_deref(), Some("cid-1"));
        assert_eq!(ov.username.as_deref(), Some("u1"));
        assert_eq!(ov.password.as_deref(), Some("p1"));
        assert_eq!(ov.keepalive, Some(60));
    }

    /// nil / 空 table / 部分字段 → 未覆盖项保持 None（cfg 缺省生效）。
    #[test]
    fn auth_overrides_nil_and_partial() {
        let lua = mlua::Lua::new();
        let ov = apply_auth_overrides(mlua::Value::Nil).unwrap();
        assert!(ov.client_id.is_none() && ov.keepalive.is_none());
        let v: mlua::Value = lua.load(r#"return { username = "u" }"#).eval().unwrap();
        let ov = apply_auth_overrides(v).unwrap();
        assert_eq!(ov.username.as_deref(), Some("u"));
        assert!(ov.client_id.is_none() && ov.password.is_none() && ov.keepalive.is_none());
    }

    /// 字段类型不符 / 非 table 返回 → Err（坏声明可感知）。
    #[test]
    fn auth_overrides_rejects_bad_types() {
        let lua = mlua::Lua::new();
        let v: mlua::Value = lua.load(r#"return { client_id = 42 }"#).eval().unwrap();
        assert!(apply_auth_overrides(v).is_err(), "client_id 非字符串应 Err");
        let v: mlua::Value = lua.load(r#"return "static-token""#).eval().unwrap();
        assert!(
            apply_auth_overrides(v).is_err(),
            "非 table/nil 返回应 Err（契约违约）"
        );
    }

    // ===== will_to_lastwill =====

    /// nil → None（无 LWT）。
    #[test]
    fn will_nil_yields_none() {
        assert!(will_to_lastwill(mlua::Value::Nil).unwrap().is_none());
    }

    /// 全字段：topic/payload 原样、qos/retain 生效。
    #[test]
    fn will_full_table_builds_lastwill() {
        let lua = mlua::Lua::new();
        let v: mlua::Value = lua
            .load(r#"return { topic = "t/offline", payload = "gone", qos = 1, retain = true }"#)
            .eval()
            .unwrap();
        let w = will_to_lastwill(v).unwrap().expect("应组出 LastWill");
        assert_eq!(w.topic, "t/offline");
        assert_eq!(&w.message[..], b"gone");
        assert_eq!(w.qos, rumqttc::QoS::AtLeastOnce);
        assert!(w.retain);
    }

    /// qos/retain 缺省：qos=0、retain=false；qos 非法值降缺省。
    #[test]
    fn will_defaults_and_bad_qos() {
        let lua = mlua::Lua::new();
        let v: mlua::Value = lua
            .load(r#"return { topic = "t", payload = "p" }"#)
            .eval()
            .unwrap();
        let w = will_to_lastwill(v).unwrap().unwrap();
        assert_eq!(w.qos, rumqttc::QoS::AtMostOnce);
        assert!(!w.retain);
        let v: mlua::Value = lua
            .load(r#"return { topic = "t", payload = "p", qos = 9 }"#)
            .eval()
            .unwrap();
        let w = will_to_lastwill(v).unwrap().unwrap();
        assert_eq!(w.qos, rumqttc::QoS::AtMostOnce, "非法 qos 降缺省 0");
    }

    // ===== build_mqtt_options =====

    fn test_cfg() -> NorthCfg {
        super::super::config::parse_north_cfg(&serde_json::json!({
            "script": "cloud1", "host": "b.example.com", "port": 8883,
            "username": "cfg-user", "client_id": "cfg-cid"
        }))
        .unwrap()
    }

    /// auth 覆盖优先：client_id/username/keepalive 全覆盖；password 走参入（已解析）。
    #[test]
    fn mqtt_options_auth_overrides_win() {
        let cfg = test_cfg();
        let ov = AuthOverrides {
            client_id: Some("auth-cid".into()),
            username: Some("auth-user".into()),
            password: Some("ignored-here".into()), // build 不消费，password 走参入
            keepalive: Some(15),
        };
        let opts = build_mqtt_options(&cfg, &ov, Some("real-pw".into()), None, "hw", "inst");
        assert_eq!(opts.client_id(), "auth-cid");
        assert_eq!(opts.keep_alive(), std::time::Duration::from_secs(15));
        let (host, port) = opts.broker_address();
        assert_eq!(host, "b.example.com");
        assert_eq!(port, 8883);
    }

    /// 无覆盖 → cfg 缺省：client_id 显式配置生效、keepalive 30s；auth 只覆盖部分时
    /// 其余回 cfg（username 回 cfg-user）。
    #[test]
    fn mqtt_options_cfg_fallback() {
        let cfg = test_cfg();
        let opts = build_mqtt_options(&cfg, &AuthOverrides::default(), None, None, "hw", "inst");
        assert_eq!(opts.client_id(), "cfg-cid", "auth 未覆盖 → cfg 显式配置");
        assert_eq!(opts.keep_alive(), std::time::Duration::from_secs(30));
        // client_id 未配置时缺省物化 hwid-<inst_id>。
        let cfg2 = super::super::config::parse_north_cfg(&serde_json::json!({
            "script": "s", "host": "h"
        }))
        .unwrap();
        let opts2 = build_mqtt_options(&cfg2, &AuthOverrides::default(), None, None, "hw-9", "n-9");
        assert_eq!(opts2.client_id(), "hw-9-n-9");
    }

    // ===== parse_up_result =====

    /// 合法数组：qos 缺省 0、显式 1/2、payload 字节串原样。
    #[test]
    fn up_result_valid_items() {
        let lua = mlua::Lua::new();
        let v: mlua::Value = lua
            .load(
                r#"return {
                    { topic = "a/1", payload = "x" },
                    { topic = "a/2", payload = "y", qos = 1 },
                    { topic = "a/3", payload = "z", qos = 2 },
                }"#,
            )
            .eval()
            .unwrap();
        let (items, fail) = parse_up_result(v).unwrap();
        assert_eq!(fail, 0);
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].topic, "a/1");
        assert_eq!(items[0].payload, b"x");
        assert_eq!(items[0].qos, rumqttc::QoS::AtMostOnce, "qos 缺省 0");
        assert_eq!(items[1].qos, rumqttc::QoS::AtLeastOnce);
        assert_eq!(items[2].qos, rumqttc::QoS::ExactlyOnce);
    }

    /// nil / 空数组 → 空 items（调用方 warn 跳过）。
    #[test]
    fn up_result_nil_and_empty() {
        let (items, fail) = parse_up_result(mlua::Value::Nil).unwrap();
        assert!(items.is_empty() && fail == 0);
        let lua = mlua::Lua::new();
        let v: mlua::Value = lua.load(r#"return {}"#).eval().unwrap();
        let (items, fail) = parse_up_result(v).unwrap();
        assert!(items.is_empty() && fail == 0);
    }

    /// 非数组返回 → Err（契约违约）。
    #[test]
    fn up_result_rejects_non_array() {
        let lua = mlua::Lua::new();
        let v: mlua::Value = lua.load(r#"return "nope""#).eval().unwrap();
        assert!(parse_up_result(v).is_err());
    }

    /// 坏条目（非 table / topic 非 string / payload 非 string）计 fail 不中断。
    #[test]
    fn up_result_counts_bad_entries() {
        let lua = mlua::Lua::new();
        let v: mlua::Value = lua
            .load(
                r#"return {
                    { topic = "ok/1", payload = "p" },
                    "raw-string",
                    { payload = "no-topic" },
                    { topic = 42, payload = "p" },
                    { topic = "ok/2", payload = 7 },
                }"#,
            )
            .eval()
            .unwrap();
        let (items, fail) = parse_up_result(v).unwrap();
        assert_eq!(items.len(), 1, "合法条目照常产出：{items:?}");
        assert_eq!(items[0].topic, "ok/1");
        assert_eq!(fail, 4, "4 条坏条目各计一次 fail");
    }

    // ===== pull_rtdata_batch / batch_to_lua（mock lux 写 rtdata 后断言形状）=====

    /// 起独立 lux（port 0 + 独立 temp data_dir，照 config.rs 测试模式）。
    async fn north_lux(tag: &str) -> (lux::EmbeddedClient, String) {
        let data_dir = std::env::temp_dir()
            .join(format!("lua-north-{tag}-{}-{}", std::process::id(), tag))
            .to_str()
            .unwrap()
            .to_string();
        let cfg = crate::config::AppConfig {
            lux: crate::config::LuxConfig {
                port: 0,
                data_dir: data_dir.clone(),
                ..Default::default()
            },
            ..Default::default()
        };
        let handle = lux::run_with_config(crate::db::build_lux_config(&cfg))
            .await
            .expect("lux");
        (handle.client(), data_dir)
    }

    /// lux pull 形状：写两个设备 rtdata（field 含 `<devid>.` 前缀）→ entries 的
    /// devid/values（tag 已剥前缀）/ts（取最大）断言。
    #[tokio::test]
    async fn pull_batch_shape_from_rtdata() {
        let (client, dir) = north_lux("pull").await;
        let mut vals = BTreeMap::new();
        vals.insert("temp".to_string(), crate::types::SampleValue::Float(25.5));
        vals.insert("hum".to_string(), crate::types::SampleValue::Int(60));
        crate::db::rtdata::write(&client, "hw", "devA", &vals, 1_700_000_100)
            .await
            .unwrap();
        let mut vals2 = BTreeMap::new();
        vals2.insert("p".to_string(), crate::types::SampleValue::Str("on".into()));
        crate::db::rtdata::write(&client, "hw", "devB", &vals2, 1_700_000_200)
            .await
            .unwrap();

        let entries = pull_rtdata_batch(&client).await.unwrap();
        assert_eq!(entries.len(), 2, "两设备各一条：{entries:?}");
        let by_devid: std::collections::HashMap<&str, &BatchEntry> =
            entries.iter().map(|e| (e.devid.as_str(), e)).collect();
        let a = by_devid.get("devA").expect("devA 在");
        assert_eq!(
            a.values.get("temp").and_then(|v| v.as_str()),
            Some("25.5"),
            "tag 剥前缀 + cell value 原样：{:?}",
            a.values
        );
        assert_eq!(a.values.get("hum").and_then(|v| v.as_str()), Some("60"));
        assert_eq!(a.ts, 1_700_000_100, "ts 取 cell 的 ts");
        let b = by_devid.get("devB").expect("devB 在");
        assert_eq!(b.values.get("p").and_then(|v| v.as_str()), Some("on"));
        assert_eq!(b.ts, 1_700_000_200);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Lua table 形状断言：lux 条目 + rx 缓冲样本合并为脚本侧 `batch[i]` 结构。
    #[test]
    fn batch_table_shape_for_script() -> mlua::Result<()> {
        let lua = mlua::Lua::new();
        let mut values = BTreeMap::new();
        values.insert("v1".to_string(), serde_json::json!(42));
        let entries = vec![BatchEntry {
            devid: "devA".into(),
            values,
            ts: 111,
        }];
        let samples = vec![Sample {
            hwid: crate::types::Hwid::new("hw"),
            devid: "devB".into(),
            values: [("t2".to_string(), crate::types::SampleValue::Bool(true))]
                .into_iter()
                .collect(),
            timestamp: 222,
        }];
        let t = batch_to_lua(&lua, &entries, &samples)?;
        let n = t.raw_len();
        assert_eq!(n, 2, "lux 条目 + rx 样本各一条");
        let d1: String = t.get::<mlua::Table>(1)?.get("devid")?;
        assert_eq!(d1, "devA");
        let v1: i64 = t
            .get::<mlua::Table>(1)?
            .get::<mlua::Table>("values")?
            .get("v1")?;
        assert_eq!(v1, 42, "JSON 值经 to_value 成 Lua number");
        let ts1: i64 = t.get::<mlua::Table>(1)?.get("ts")?;
        assert_eq!(ts1, 111);
        let d2: String = t.get::<mlua::Table>(2)?.get("devid")?;
        assert_eq!(d2, "devB", "rx 缓冲样本并入 batch");
        let t2: bool = t
            .get::<mlua::Table>(2)?
            .get::<mlua::Table>("values")?
            .get("t2")?;
        assert!(t2, "SampleValue::Bool → Lua boolean");
        Ok(())
    }

    // ===== resolve_password_opt（框架测试迁自 config.rs T6）=====

    #[tokio::test]
    async fn resolve_password_framework() {
        let (client, dir) = north_lux("pw").await;
        // None → None。
        assert_eq!(resolve_password_opt(None, &client).await.unwrap(), None);
        // 字面值 → 原样。
        assert_eq!(
            resolve_password_opt(Some("plain"), &client).await.unwrap(),
            Some("plain".into())
        );
        // 引用且已写 → 明文。
        crate::db::secrets::set(&client, "secret:broker_pw", "real-pw")
            .await
            .unwrap();
        assert_eq!(
            resolve_password_opt(Some("secret:broker_pw"), &client)
                .await
                .unwrap(),
            Some("real-pw".into())
        );
        // 引用但未写 → Ok(None)（连接序列按认证失败处理）。
        assert_eq!(
            resolve_password_opt(Some("secret:absent"), &client)
                .await
                .unwrap(),
            None
        );
        // 非法 ref（大写）→ Err（与 ctx:secret_get 同规则）。
        assert!(resolve_password_opt(Some("secret:Bad_Ref"), &client)
            .await
            .is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ===== up 预算（照 lua_client collect_budget_timeout 模式）=====

    /// up 内挂起（永不返回的宿主函数）→ 预算切断 Err；重定义快路径后 VM 仍活
    /// （超时跳过本轮、下一轮仍响应；协程弃留 await 点，要求脚本幂等）。
    #[tokio::test]
    async fn up_budget_timeout_skips_round_and_vm_stays_usable() {
        let lua = mlua::Lua::new();
        lua.globals()
            .set(
                "hang",
                lua.create_async_function(|_, _: mlua::Value| async move {
                    std::future::pending::<mlua::Result<()>>().await
                })
                .unwrap(),
            )
            .unwrap();
        lua.load("function up(batch) hang() end").exec().unwrap();
        let batch = lua.create_table().unwrap();
        let started = std::time::Instant::now();
        let r = with_budget("up", 200, call_up(&lua, batch)).await;
        assert!(r.is_err(), "挂起 up 应在预算处被切断：{r:?}");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "应在 200ms 预算切断而非挂死"
        );
        // 切断后下一轮仍活：快路径 up 正常返回。
        lua.load("function up(batch) return {} end").exec().unwrap();
        let batch = lua.create_table().unwrap();
        let v = with_budget("up", 1_000, call_up(&lua, batch))
            .await
            .expect("下一轮 up 应正常")
            .expect("快路径 up 返回值透传");
        let (items, _) = parse_up_result(v).unwrap();
        assert!(items.is_empty());
    }

    // ===== 集成：auth 重调（连必失败端口 127.0.0.1:1，无需真实 broker）=====

    /// Q5 核心行为锁：连接失败 → 断连事件 → 退避一个 period → 重连序列**重调
    /// auth()**（凭证刷新生效的前提）。脚本侧 kv 计数 auth 调用次数，等 >1 个重连
    /// 周期后断言 ≥2。
    #[tokio::test]
    async fn reconnect_reinvokes_auth() {
        use crate::host::{BridgeRegistry, CtlRegistry, HostContext};
        use crate::plugin::traits::Sink;
        use crate::types::{AppType, DeviceSpec, HostKind, Hwid, InstConfig, PluginName};
        use serde_json::json;

        let tmp = std::env::temp_dir().join(format!("lua-north-auth-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("cloudN")).unwrap();
        std::fs::write(
            tmp.join("cloudN/main.lua"),
            "-- @role mqtt-north\n\
             function auth(cfg)\n  local n = (ctx:kv_get('calls') or 0) + 1\n  ctx:kv_set('calls', n)\n  return {}\n\
             end\n\
             function up(batch) return {} end",
        )
        .unwrap();

        let data_dir = std::env::temp_dir()
            .join(format!("lua-north-auth-lux-{}", std::process::id()))
            .to_str()
            .unwrap()
            .to_string();
        let cfg_lux = crate::config::AppConfig {
            lux: crate::config::LuxConfig {
                port: 0,
                data_dir: data_dir.clone(),
                ..Default::default()
            },
            ..Default::default()
        };
        let handle = lux::run_with_config(crate::db::build_lux_config(&cfg_lux))
            .await
            .expect("lux");
        let client = handle.client();

        let (_tx, rx) = mpsc::channel::<Sample>(8); // Sender 不消费（north 是 Sink 消费者）
        let (_cmd_tx, cmd_rx) = mpsc::channel::<crate::types::CtlMessage>(4);
        let ctx = HostContext {
            lux: client.clone(),
            hwid: Hwid::new("it"),
            shutdown: CancellationToken::new(),
            ctl_registry: CtlRegistry::new(std::sync::Arc::new(
                crate::process_metrics::ProcessMetrics::new(),
            )),
            bridge_registry: BridgeRegistry::new(),
            metrics: std::sync::Arc::new(crate::process_metrics::ProcessMetrics::new()),
            trace_registry: crate::io_trace::TraceRegistry::new(),
            event_tx: crate::event_bus::EventBus::dummy(),
        };
        let inst = InstConfig {
            inst_id: "north-auth-it".into(),
            inst_name: "it".into(),
            app_code: PluginName::new("luaNorth"),
            app_type: AppType::North,
            devices: vec![DeviceSpec {
                label: "d".into(),
                value: "devN".into(),
                children: vec![],
                slave_id: None,
            }],
            app_config: json!({
                "script": "cloudN", "host": "127.0.0.1", "port": 1,
                "period": 1, "batch_max_secs": 1,
                "scripts_root": tmp.to_str().unwrap()
            }),
            auto_start: false,
            host_kind: HostKind::Builtin,
        };
        let token = CancellationToken::new();
        let run_token = token.child_token();
        let inst_id = inst.inst_id.to_string();
        tokio::spawn(async move {
            crate::plugin::builtin::lua_north::LuaNorth
                .run(ctx, inst, rx, cmd_rx, run_token)
                .await;
        });

        // 等 >1 个重连周期（period=1s）：初始连接失败 → 退避 1s → 重连重调 auth。
        tokio::time::sleep(std::time::Duration::from_millis(3_500)).await;
        let calls = crate::db::luakv::get(&client, &inst_id, "calls")
            .await
            .unwrap();
        token.cancel();
        let _ = std::fs::remove_dir_all(&tmp);
        let n: i64 = calls
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or(0);
        assert!(
            n >= 2,
            "断连后重连应重调 auth()（Q5）：实测 auth 调用 {n} 次（期望 ≥2）"
        );
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    // ===== e2e（真 broker，默认 ignore；照 e2e_sync_official_dist_over_http 模式）=====

    /// T7 连接成功路径的契约闭环（本机无 broker 故 ignore）：ConnAck → on_connack →
    /// subscribe → up() 发布（对端收到）→ 对端 publish 下行 → down() 收到（kv 落痕）
    /// → 断连 → on_disconnect。
    ///
    /// 跑法（需本机真实 broker，如 mosquitto / EMQX）：
    /// ```text
    /// # 1) 起 broker（默认 1883），确认允许匿名连接
    /// # 2) RSIOT_NORTH_E2E_BROKER=127.0.0.1:1883 \
    /// #    cargo test -p rs-iot --features lua,lua-http lua_north -- --ignored
    /// ```
    /// 验证链：脚本 kv 的 connacks/down_got + 对端收到的 up payload。
    #[ignore = "需本机真实 broker（mosquitto/EMQX）+ RSIOT_NORTH_E2E_BROKER 环境变量，见测试注释"]
    #[tokio::test]
    async fn e2e_north_contract_over_broker() {
        use crate::host::{BridgeRegistry, CtlRegistry, HostContext};
        use crate::plugin::traits::Sink;
        use crate::types::{AppType, DeviceSpec, HostKind, Hwid, InstConfig, PluginName};
        use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS};
        use serde_json::json;

        let broker =
            std::env::var("RSIOT_NORTH_E2E_BROKER").expect("需 RSIOT_NORTH_E2E_BROKER=host:port");
        let (host, port) = broker
            .split_once(':')
            .expect("RSIOT_NORTH_E2E_BROKER 需 host:port 形态");
        let port: u16 = port.parse().expect("port 数字");

        // 脚本：auth 覆盖 client_id；up 发布固定 topic；down/on_connack/on_disconnect
        // 落 kv（断言用）。
        let tmp = std::env::temp_dir().join(format!("lua-north-e2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("cloudE2E")).unwrap();
        std::fs::write(
            tmp.join("cloudE2E/main.lua"),
            r#"-- @role mqtt-north
local UP = "rsiot/e2e/north/up"
local CMD = "rsiot/e2e/north/cmd"
function auth(cfg) return { client_id = "rsiot-north-e2e" } end
function up(batch) return { { topic = UP, payload = "hello-north" } } end
function down(topic, payload) ctx:kv_set("down_got", topic .. "|" .. payload) end
function on_connack()
  local n = (ctx:kv_get("connacks") or 0) + 1
  ctx:kv_set("connacks", n)
end
function on_disconnect()
  local n = (ctx:kv_get("disconnects") or 0) + 1
  ctx:kv_set("disconnects", n)
end"#,
        )
        .unwrap();

        let data_dir = std::env::temp_dir()
            .join(format!("lua-north-e2e-lux-{}", std::process::id()))
            .to_str()
            .unwrap()
            .to_string();
        let cfg_lux = crate::config::AppConfig {
            lux: crate::config::LuxConfig {
                port: 0,
                data_dir: data_dir.clone(),
                ..Default::default()
            },
            ..Default::default()
        };
        let handle = lux::run_with_config(crate::db::build_lux_config(&cfg_lux))
            .await
            .expect("lux");
        let client = handle.client();

        let inst = InstConfig {
            inst_id: "north-e2e".into(),
            inst_name: "e2e".into(),
            app_code: PluginName::new("luaNorth"),
            app_type: AppType::North,
            devices: vec![DeviceSpec {
                label: "d".into(),
                value: "devE".into(),
                children: vec![],
                slave_id: None,
            }],
            app_config: json!({
                "script": "cloudE2E", "host": host, "port": port,
                "period": 1, "batch_max_secs": 1,
                "subscribe": ["rsiot/e2e/north/cmd"],
                "scripts_root": tmp.to_str().unwrap()
            }),
            auto_start: false,
            host_kind: HostKind::Builtin,
        };
        let (_tx, rx) = mpsc::channel::<Sample>(8);
        let (_cmd_tx, cmd_rx) = mpsc::channel::<crate::types::CtlMessage>(4);
        let token = CancellationToken::new();
        let run_token = token.child_token();
        let ctx = HostContext {
            lux: client.clone(),
            hwid: Hwid::new("it"),
            shutdown: CancellationToken::new(),
            ctl_registry: CtlRegistry::new(std::sync::Arc::new(
                crate::process_metrics::ProcessMetrics::new(),
            )),
            bridge_registry: BridgeRegistry::new(),
            metrics: std::sync::Arc::new(crate::process_metrics::ProcessMetrics::new()),
            trace_registry: crate::io_trace::TraceRegistry::new(),
            event_tx: crate::event_bus::EventBus::dummy(),
        };
        tokio::spawn(async move {
            crate::plugin::builtin::lua_north::LuaNorth
                .run(ctx, inst, rx, cmd_rx, run_token)
                .await;
        });

        // 对端客户端：订阅 up topic（验 north 发布）；ConnAck 后向 cmd topic 下发。
        let mut peer = MqttOptions::new("rsiot-e2e-peer", host, port);
        peer.set_keep_alive(std::time::Duration::from_secs(30));
        let (peer_tx, mut peer_ev) = AsyncClient::new(peer, 10);
        peer_tx
            .subscribe("rsiot/e2e/north/up", QoS::AtMostOnce)
            .await
            .expect("peer subscribe");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let mut got_up = false;
        let mut sent_cmd = false;
        while std::time::Instant::now() < deadline && (!got_up || !sent_cmd) {
            match tokio::time::timeout(std::time::Duration::from_secs(1), peer_ev.poll()).await {
                Ok(Ok(Event::Incoming(Packet::ConnAck(_)))) => {
                    // 对端连上即向 down topic 发一条（north 订阅在自身 ConnAck 后）。
                    peer_tx
                        .publish("rsiot/e2e/north/cmd", QoS::AtMostOnce, false, b"ping")
                        .await
                        .unwrap();
                    sent_cmd = true;
                }
                Ok(Ok(Event::Incoming(Packet::Publish(p)))) if p.topic == "rsiot/e2e/north/up" => {
                    assert_eq!(&p.payload[..], b"hello-north", "up() 发布的 payload");
                    got_up = true;
                }
                Ok(_) => {}
                Err(_) => {} // 1s 窗口无事件，继续到 deadline
            }
        }
        assert!(got_up, "对端应收 north up() 的发布（20s 内）");
        assert!(sent_cmd);

        // down 落痕（脚本 kv）：等 north 的 down() 执行。
        let mut down_got = None;
        let kv_deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < kv_deadline {
            if let Some(s) = crate::db::luakv::get(&client, "north-e2e", "down_got")
                .await
                .unwrap()
            {
                down_got = Some(s);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        }
        assert_eq!(
            down_got.as_deref().map(|s| s.trim_matches('"')),
            Some("rsiot/e2e/north/cmd|ping"),
            "down(topic, payload) 应收到并落 kv"
        );
        // on_connack 落痕（连接成功回调被调过）。
        let connacks = crate::db::luakv::get(&client, "north-e2e", "connacks")
            .await
            .unwrap();
        assert!(
            connacks.is_some_and(|s| s.contains('1')),
            "on_connack 应被调（kv connacks）"
        );

        token.cancel();
        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    // ===== e2e（进程内 rumqttd broker，默认套件，T10）=====

    /// 起进程内 rumqttd（明文 TCP v4 listener，127.0.0.1 随机端口）。
    ///
    /// 端口选法：先 bind `:0` 拿空闲口再释放交给 rumqttd（其 `TcpListener` 在自有
    /// 线程内 bind，无 local_addr 查询口）——微小 TOCTOU 测试可忍，就绪探测失败即
    /// panic。0.20 的 `Broker::start` 无停机口（阻塞线程常驻，测试结束随进程回收），
    /// 断连编排走同 client_id 抢占（rumqttd 默认 takeover 语义）而非停 broker——
    /// 见 [`e2e_north_contract_inproc_broker`] 文档。
    fn spawn_inproc_broker(tag: &str) -> u16 {
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind :0 选空闲端口");
            l.local_addr().expect("local_addr").port()
        };
        // RouterConfig 需显式非零（derive Default 全 0——max_connections=0 会拒一切连接）
        let config = rumqttd::Config {
            router: rumqttd::RouterConfig {
                max_connections: 64,
                max_outgoing_packet_count: 200,
                max_segment_size: 10 * 1024 * 1024,
                max_segment_count: 10,
                custom_segment: None,
                initialized_filters: None,
                shared_subscriptions_strategy: Default::default(),
            },
            v4: Some(
                [(
                    "test".to_string(),
                    rumqttd::ServerSettings {
                        name: format!("e2e-{tag}"),
                        listen: format!("127.0.0.1:{port}").parse().expect("SocketAddr"),
                        tls: None,
                        next_connection_delay_ms: 1,
                        connections: rumqttd::ConnectionSettings {
                            connection_timeout_ms: 60_000,
                            max_payload_size: 1024 * 1024,
                            max_inflight_count: 100,
                            auth: None, // 匿名放行（ConnectionSettings 无 Default derive）
                            external_auth: None,
                            dynamic_filters: false,
                        },
                    },
                )]
                .into_iter()
                .collect(),
            ),
            ..Default::default()
        };
        let mut broker = rumqttd::Broker::new(config);
        std::thread::Builder::new()
            .name(format!("rumqttd-{tag}"))
            .spawn(move || {
                if let Err(e) = broker.start() {
                    eprintln!("[e2e] rumqttd start 失败：{e:?}");
                }
            })
            .expect("spawn rumqttd 线程");
        // 就绪探测：TCP 连通即 listener up（5s 上限）
        for _ in 0..100 {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                return port;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        panic!("rumqttd 5s 未就绪（port {port}）");
    }

    /// 复审阻塞-1 回归（2026-08-20 阶段 B 深度审查）：单批 publish 超过 flume 通道
    /// 容量（旧 16/新 64）不再死锁——旧单任务形态 publish 在 select 臂内 await 而
    /// poll 被挂起，第 17 条起永久阻塞且 token 不可达（默认兼容实例 17 设备即触）。
    /// 双任务重构后 pump 并发排空；本测试 up() 返回 **80 条**（> 任何一档容量），
    /// 断言对端订阅者全数收到——死锁形态下最多收到 16 条且实例停摆。
    #[tokio::test]
    async fn publish_batch_over_channel_cap_no_deadlock() {
        use crate::host::{BridgeRegistry, CtlRegistry, HostContext};
        use crate::plugin::traits::Sink;
        use crate::types::{AppType, DeviceSpec, HostKind, Hwid, InstConfig, PluginName};
        use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS};
        use serde_json::json;

        const UP: &str = "rsiot/e2e/north/burst";
        const N: usize = 80; // > REQUEST_CAP(64) > 旧死锁容量 16

        let port = spawn_inproc_broker("north-burst");
        let tmp = std::env::temp_dir().join(format!("lua-north-burst-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("burstN")).unwrap();
        std::fs::write(
            tmp.join("burstN/main.lua"),
            format!(
                r#"-- @role mqtt-north
local UP = "{UP}"
local N = {N}
function auth(cfg) return nil end
function up(batch)
  local out = {{}}
  for i = 1, N do
    out[#out + 1] = {{ topic = UP, payload = "m-" .. i, qos = 0 }}
  end
  return out
end
function down(topic, payload) end"#
            ),
        )
        .unwrap();

        let data_dir = std::env::temp_dir()
            .join(format!("lua-north-burst-lux-{}", std::process::id()))
            .to_str()
            .unwrap()
            .to_string();
        let cfg_lux = crate::config::AppConfig {
            lux: crate::config::LuxConfig {
                port: 0,
                data_dir,
                ..Default::default()
            },
            ..Default::default()
        };
        let handle = lux::run_with_config(crate::db::build_lux_config(&cfg_lux))
            .await
            .expect("lux");
        let client = handle.client();

        let inst = InstConfig {
            inst_id: "north-burst".into(),
            inst_name: "burst".into(),
            app_code: PluginName::new("luaNorth"),
            app_type: AppType::North,
            devices: vec![DeviceSpec {
                label: "d".into(),
                value: "devB".into(),
                children: vec![],
                slave_id: None,
            }],
            app_config: json!({
                "script": "burstN", "host": "127.0.0.1", "port": port,
                "period": 1, "batch_max_secs": 1,
                "client_id": "north-burst-it",
                "scripts_root": tmp.to_str().unwrap()
            }),
            auto_start: false,
            host_kind: HostKind::Builtin,
        };
        let (tx, rx) = mpsc::channel::<Sample>(8);
        let (_cmd_tx, cmd_rx) = mpsc::channel::<crate::types::CtlMessage>(4);
        let token = CancellationToken::new();
        let run_token = token.child_token();
        let ctx = HostContext {
            lux: client.clone(),
            hwid: Hwid::new("it"),
            shutdown: CancellationToken::new(),
            ctl_registry: CtlRegistry::new(std::sync::Arc::new(
                crate::process_metrics::ProcessMetrics::new(),
            )),
            bridge_registry: BridgeRegistry::new(),
            metrics: std::sync::Arc::new(crate::process_metrics::ProcessMetrics::new()),
            trace_registry: crate::io_trace::TraceRegistry::new(),
            event_tx: crate::event_bus::EventBus::dummy(),
        };
        tokio::spawn(async move {
            crate::plugin::builtin::lua_north::LuaNorth
                .run(ctx, inst, rx, cmd_rx, run_token)
                .await;
        });
        // 喂一条样本触发 up()（up() 忽略 batch 内容，固定返回 N 条）
        tx.send(Sample {
            hwid: crate::types::Hwid::new("it"),
            devid: "devB".into(),
            values: [("v".to_string(), crate::types::SampleValue::Int(1))]
                .into_iter()
                .collect(),
            timestamp: 1,
        })
        .await
        .unwrap();

        // 对端订阅者收满 N 条（死锁形态：收到 ≤16 后永久停滞）
        let mut peer = MqttOptions::new("rsiot-burst-peer", "127.0.0.1", port);
        peer.set_keep_alive(std::time::Duration::from_secs(30));
        let (peer_tx, mut peer_ev) = AsyncClient::new(peer, 10);
        // 等 broker 就绪 + 订阅生效（ConnAck 后重试订阅直至成功）
        for _ in 0..20 {
            if peer_tx.subscribe(UP, QoS::AtMostOnce).await.is_ok() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }

        let mut got = 0usize;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while got < N && std::time::Instant::now() < deadline {
            match tokio::time::timeout(std::time::Duration::from_secs(2), peer_ev.poll()).await {
                Ok(Ok(Event::Incoming(Packet::Publish(p)))) => {
                    if p.topic == UP {
                        got += 1;
                    }
                }
                Ok(Ok(_)) => {}
                Ok(Err(e)) => panic!("peer poll err: {e}"),
                Err(_) => break, // 总超时——跳出后由断言给结论
            }
        }
        assert_eq!(got, N, "80 条批量发布应全数送达（死锁回归），实得 {got}");
        token.cancel();
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// T10 契约 e2e（进程内 rumqttd 0.20，默认跑——无需外部 broker/环境变量，
    /// 真 broker 现场版见 [`e2e_north_contract_over_broker`]）。
    ///
    /// 覆盖清单（T7 契约全链路 + LWT + 重连）：
    /// 1. ConnAck → on_connack 落 kv；cfg.subscribe 订阅生效；
    /// 2. up() 发布 → 对端订阅者收到 payload（hello-n，n 为 kv 计数）；
    /// 3. 对端 publish 下行 topic → down(topic, payload) 落 kv；
    /// 4. **断连编排**：helper 以同 client_id 连入（rumqttd 默认 takeover——旧连接
    ///    被路由器丢弃且 TCP 关闭，等效"断 broker"侧的连接中断）→
    ///    a) 遗嘱：will() 配置的 topic 上对端收到 "offline"（LWT，clean_session
    ///       下 rumqttd 立即发布）；
    ///    b) on_disconnect 落 kv；
    /// 5. 退避 1 个 period 后重连（重调 auth）→ connacks ≥ 2；
    /// 6. 重连后再发布一批 → 对端收到 hello-n（n ≥ 2，断连后新批次）。
    ///
    /// 脚本侧 auth 委托 `require("cloud.emqx")` 配方（保留段 require 路由进 e2e：
    /// `_stdlib/cloud/emqx.lua` 直写——分发验签链路有 stdlib_sync 专项测试）。
    #[tokio::test]
    async fn e2e_north_contract_inproc_broker() {
        use crate::host::{BridgeRegistry, CtlRegistry, HostContext};
        use crate::plugin::traits::Sink;
        use crate::types::{AppType, DeviceSpec, HostKind, Hwid, InstConfig, PluginName};
        use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS};
        use serde_json::json;

        const UP: &str = "rsiot/e2e/north/up";
        const CMD: &str = "rsiot/e2e/north/cmd";
        const WILL: &str = "rsiot/e2e/north/will";
        const CLIENT_ID: &str = "north-e2e-inproc";

        let port = spawn_inproc_broker("north-e2e");

        // 脚本根：main.lua + `_stdlib/cloud/emqx.lua`（保留段 require 路由面；
        // stdlib 验签 sync 分发链路有 stdlib_sync.rs 专项测试，此处直写保持
        // e2e 对 feature 组合自包含——minimal `--features lua` 同样默认跑）。
        let tmp = std::env::temp_dir().join(format!("lua-north-e2e2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("cloudE2E2")).unwrap();
        std::fs::create_dir_all(tmp.join("_stdlib/cloud")).unwrap();
        std::fs::write(
            tmp.join("_stdlib/cloud/emqx.lua"),
            // 合成 emqx stub（auth 恒 nil + topic helper——真实配方 API 面等价子集）
            "local M = {}\n\
             function M.auth(_cfg) return nil end\n\
             function M.topic_data(hw, devid) return hw .. '/' .. devid .. '/datas' end\n\
             function M.selftest() return M.topic_data('a', 'b') == 'a/b/datas' end\n\
             return M\n",
        )
        .unwrap();
        std::fs::write(
            tmp.join("cloudE2E2/main.lua"),
            format!(
                r#"-- @role mqtt-north
local em = require("cloud.emqx")
local UP = "{UP}"
function auth(cfg)
  -- EMQX 裸 broker 配方：无平台认证（恒 nil）；固定 client_id 供 takeover 断连编排
  assert(em.auth(cfg) == nil)
  return {{ client_id = "{CLIENT_ID}" }}
end
function will() return {{ topic = "{WILL}", payload = "offline" }} end
function up(batch)
  local n = (ctx:kv_get("ups") or 0) + 1
  ctx:kv_set("ups", n)
  return {{ {{ topic = UP, payload = "hello-" .. n }} }}
end
function down(topic, payload) ctx:kv_set("down_got", topic .. "|" .. payload) end
function on_connack()
  local n = (ctx:kv_get("connacks") or 0) + 1
  ctx:kv_set("connacks", n)
end
function on_disconnect()
  local n = (ctx:kv_get("disconnects") or 0) + 1
  ctx:kv_set("disconnects", n)
end"#
            ),
        )
        .unwrap();

        let data_dir = std::env::temp_dir()
            .join(format!("lua-north-e2e2-lux-{}", std::process::id()))
            .to_str()
            .unwrap()
            .to_string();
        let cfg_lux = crate::config::AppConfig {
            lux: crate::config::LuxConfig {
                port: 0,
                data_dir: data_dir.clone(),
                ..Default::default()
            },
            ..Default::default()
        };
        let handle = lux::run_with_config(crate::db::build_lux_config(&cfg_lux))
            .await
            .expect("lux");
        let client = handle.client();

        let inst = InstConfig {
            inst_id: "north-e2e2".into(),
            inst_name: "e2e2".into(),
            app_code: PluginName::new("luaNorth"),
            app_type: AppType::North,
            devices: vec![DeviceSpec {
                label: "d".into(),
                value: "devE".into(),
                children: vec![],
                slave_id: None,
            }],
            app_config: json!({
                "script": "cloudE2E2", "host": "127.0.0.1", "port": port,
                "period": 1, "batch_max_secs": 1,
                "subscribe": [CMD],
                "client_id": CLIENT_ID,
                "scripts_root": tmp.to_str().unwrap()
            }),
            auto_start: false,
            host_kind: HostKind::Builtin,
        };
        let (tx, rx) = mpsc::channel::<Sample>(8); // 保有 tx 喂样例（驱动 up() 批量）
        let (_cmd_tx, cmd_rx) = mpsc::channel::<crate::types::CtlMessage>(4);
        let token = CancellationToken::new();
        let run_token = token.child_token();
        let ctx = HostContext {
            lux: client.clone(),
            hwid: Hwid::new("it"),
            shutdown: CancellationToken::new(),
            ctl_registry: CtlRegistry::new(std::sync::Arc::new(
                crate::process_metrics::ProcessMetrics::new(),
            )),
            bridge_registry: BridgeRegistry::new(),
            metrics: std::sync::Arc::new(crate::process_metrics::ProcessMetrics::new()),
            trace_registry: crate::io_trace::TraceRegistry::new(),
            event_tx: crate::event_bus::EventBus::dummy(),
        };
        tokio::spawn(async move {
            crate::plugin::builtin::lua_north::LuaNorth
                .run(ctx, inst, rx, cmd_rx, run_token)
                .await;
        });
        // 首批样例（up() 批量数据源；断连期缓冲、重连后补发由实例语义保证）
        tx.send(Sample {
            hwid: crate::types::Hwid::new("it"),
            devid: "devE".into(),
            values: [("v".to_string(), crate::types::SampleValue::Int(1))]
                .into_iter()
                .collect(),
            timestamp: 1,
        })
        .await
        .unwrap();

        // 对端订阅者：收 up 发布 + 遗嘱；ConnAck 后周期性发下行（规避订阅时序竞态）。
        let mut peer = MqttOptions::new("rsiot-e2e2-peer", "127.0.0.1", port);
        peer.set_keep_alive(std::time::Duration::from_secs(30));
        let (peer_tx, mut peer_ev) = AsyncClient::new(peer, 10);
        peer_tx
            .subscribe(UP, QoS::AtMostOnce)
            .await
            .expect("sub UP");
        peer_tx
            .subscribe(WILL, QoS::AtMostOnce)
            .await
            .expect("sub WILL");

        let mut ups_received: Vec<String> = Vec::new();
        let mut will_got = false;
        let mut peer_connacked = false;

        // 阶段 1（20s）：up 发布到达 + down 落 kv。
        let kv_get_num = |client: &lux::EmbeddedClient, key: &str| {
            let client = client.clone();
            let key = key.to_string();
            async move {
                crate::db::luakv::get(&client, "north-e2e2", &key)
                    .await
                    .unwrap()
                    .and_then(|s| serde_json::from_str::<i64>(&s).ok())
                    .unwrap_or(0)
            }
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let mut down_got = false;
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(std::time::Duration::from_millis(500), peer_ev.poll()).await
            {
                Ok(Ok(Event::Incoming(Packet::ConnAck(_)))) => {
                    peer_connacked = true;
                }
                Ok(Ok(Event::Incoming(Packet::Publish(p)))) if p.topic == UP => {
                    ups_received.push(String::from_utf8_lossy(&p.payload).into_owned());
                }
                Ok(Ok(Event::Incoming(Packet::Publish(p)))) if p.topic == WILL => {
                    assert_eq!(&p.payload[..], b"offline", "遗嘱 payload");
                    will_got = true;
                }
                _ => {}
            }
            // 对端就绪后周期发下行（1s 一发，直到 down 落痕——容忍 north 订阅晚于首发）
            if peer_connacked && !down_got {
                let _ = peer_tx.publish(CMD, QoS::AtMostOnce, false, b"ping").await;
            }
            down_got = crate::db::luakv::get(&client, "north-e2e2", "down_got")
                .await
                .unwrap()
                .is_some();
            if !ups_received.is_empty() && down_got {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        assert!(!ups_received.is_empty(), "对端应收 up() 发布（20s 内）");
        assert!(
            ups_received[0].starts_with("hello-"),
            "up payload 形状：{:?}",
            ups_received
        );
        assert_eq!(
            crate::db::luakv::get(&client, "north-e2e2", "down_got")
                .await
                .unwrap()
                .as_deref()
                .map(|s| s.trim_matches('"')),
            Some("rsiot/e2e/north/cmd|ping"),
            "down(topic, payload) 应收到并落 kv"
        );

        // 阶段 2：takeover 断连（同 client_id 连入 → 旧连接被丢弃）。
        let mut helper = MqttOptions::new(CLIENT_ID, "127.0.0.1", port);
        helper.set_keep_alive(std::time::Duration::from_secs(30));
        let (_helper_tx, mut helper_ev) = AsyncClient::new(helper, 10);
        tokio::spawn(async move {
            // 只泵事件使 CONNECT 写出（连入即触发 takeover；随后被 north 重连反抢占）
            while helper_ev.poll().await.is_ok() {}
        });

        // 喂第二批样例（重连后 up() 再发布一批的数据源）。
        tx.send(Sample {
            hwid: crate::types::Hwid::new("it"),
            devid: "devE".into(),
            values: [("v".to_string(), crate::types::SampleValue::Int(2))]
                .into_iter()
                .collect(),
            timestamp: 2,
        })
        .await
        .unwrap();

        // 阶段 3（20s）：遗嘱 + on_disconnect + 重连（connacks≥2）+ 再发布（n≥2）。
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            match tokio::time::timeout(std::time::Duration::from_millis(500), peer_ev.poll()).await
            {
                Ok(Ok(Event::Incoming(Packet::Publish(p)))) if p.topic == UP => {
                    ups_received.push(String::from_utf8_lossy(&p.payload).into_owned());
                }
                Ok(Ok(Event::Incoming(Packet::Publish(p)))) if p.topic == WILL => {
                    assert_eq!(&p.payload[..], b"offline", "遗嘱 payload");
                    will_got = true;
                }
                _ => {}
            }
            let connacks = kv_get_num(&client, "connacks").await;
            let disconnects = kv_get_num(&client, "disconnects").await;
            let ups = kv_get_num(&client, "ups").await;
            let republished = ups_received
                .iter()
                .any(|p| p.trim_start_matches("hello-").parse::<i64>().unwrap_or(0) >= 2);
            if will_got && disconnects >= 1 && connacks >= 2 && republished {
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!(
                    "断连→重连链未闭合：will_got={will_got} disconnects={disconnects} \
                     connacks={connacks} ups(kv)={ups} 对端收到={ups_received:?}"
                );
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        // 重连订阅仍生效（takeover后新连接重新 subscribe：再发一条下行也该落 kv；
        // 周期重发规避订阅/发布时序竞态——同阶段 1 的 down 重试策略。**必须持续
        // poll peer_ev**：AsyncClient 有界请求通道（10）只由 eventloop 排空，不 poll
        // 则第 11 次 publish().await 永久阻塞）。
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let _ = peer_tx.publish(CMD, QoS::AtMostOnce, false, b"ping2").await;
            if let Ok(Ok(Event::Incoming(Packet::Publish(p)))) =
                tokio::time::timeout(std::time::Duration::from_millis(200), peer_ev.poll()).await
            {
                if p.topic == UP {
                    ups_received.push(String::from_utf8_lossy(&p.payload).into_owned());
                }
                // WILL 分支已在阶段 3 断言过（will_got 此后不再消费）
            }
            let v = crate::db::luakv::get(&client, "north-e2e2", "down_got")
                .await
                .unwrap()
                .and_then(|s| serde_json::from_str::<String>(&s).ok());
            if v.as_deref() == Some("rsiot/e2e/north/cmd|ping2") {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "重连后订阅应重新生效（down_got={v:?}）"
            );
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        }

        token.cancel();
        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::remove_dir_all(&data_dir);
    }
}
