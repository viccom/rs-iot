# `/metrics` Prometheus 端点设计（P1 可观测体系 · 子系统 1）

> scenario-gap §8 #3「P1 可观测体系」的第一块。P1 含三个独立子系统——`/metrics`、报文 trace 开关、
> 日志 ring buffer——实现机制与代码区域不重叠,经拆解决策**各自独立 spec / plan / 实现**。
> 本 spec 只覆盖 **`/metrics`**(最轻,M4 数据源已就绪)。报文 trace / 日志 ring buffer 后续单独立 spec。
>
> 事实依据来自代码核实(M4 实现 + `InstanceStat` 字段 + sysinfo 采样现状)。

## 1. 背景与定位

scenario-gap §4「埋点监测」P1 行:`/metrics`(Prometheus 文本格式)+ 进程资源指标,对接现有监控栈。
其中**进程资源指标(CPU/mem/uptime)M4 已落地**(进 `/sysstatus` 的 `process` 字段)。
故本 spec 只剩 `/metrics` 端点本身——把 M4 已埋的指标以 **Prometheus exposition 文本格式**输出。

M4 完成时已为本端点**前瞻命名**(counter 用 `_count`、耗时 `_ms`、字节 `_bytes`),
确保 `/metrics` 落地时数据源零重构,只需格式化输出。

### 三种消费者关系(澄清价值)

M4 已有的 JSON REST(`/sysstatus`、`/inst/stats`)与 `/metrics` 是**同一批数据源的三种消费者**:

| | `/sysstatus` `/inst/stats`(M4 已做) | `/metrics`(本 spec) |
|---|---|---|
| 格式 | JSON 字段平铺 | Prometheus 文本(`# HELP`/`# TYPE`/`metric{labels} value`) |
| 消费方 | 人 curl / 简单脚本 | Prometheus server 自动 scrape |
| 拉取 | 按需(人触发) | 定时 pull(Prometheus 每 15s 抓) |
| 下游 | 无 | Prometheus 时序库 → Grafana 大屏 → Alertmanager 告警 |
| 语义 | 数字字段 | metric type(counter 单调增 / gauge 可增减)+ labels |

`/metrics` 不是重新埋点,是给 M4 数据加「标准化的机器可读出口」。

## 2. 目标 / 非目标

**目标**:
- `GET /metrics` 暴露 M4 全部进程级 + per-instance 指标 + 进程资源,格式符合 Prometheus exposition standard
- 免 JWT,内网 Prometheus 可直接 scrape
- 零新增依赖(手写文本格式化)
- `/sysstatus` 与 `/metrics` 的 sysinfo 采样抽公共函数(DRY)

**非目标**(后置):
- histogram / summary(请求延迟分布)——P1 只要 counter/gauge
- 报文 trace、日志 ring buffer——独立 spec
- `InstanceStat.extra` 动态字段(opcua session_reconnect_count、lua vm_restart_count 等)的暴露——
  extra 是 flatten 任意 JSON,命名不可控;v1 只暴露核心 6 字段。若需标准化某 extra 字段,后续提升为
  核心 field 或加专用 metric
- TLS / mTLS / IP 白名单——PoC 免认证,跨网部署后置
- Grafana 仪表板 / Alertmanager 规则——下游消费方,不在 rs-iot 范围

## 3. 架构

- **端点**:`GET /metrics`(顶层路径,Prometheus 惯例;scrape 默认抓 `/metrics`,不用 `/api/v1` 前缀)
- **认证**:免 JWT,加入 `auth.rs` 的 `PUBLIC_PATHS` 白名单
- **实现**:手写 Prometheus exposition 文本格式(零依赖)
- **数据源**:M4 已就绪三处——
  - `ProcessMetrics`(`AppState.metrics`,atomic 直读,无 lux 往返)
  - lux `stat:<inst_id>`(per-instance `InstanceStat`)
  - sysinfo(进程 CPU/mem;uptime 从 `ProcessMetrics.started.elapsed()`)
- **文件**:
  - 新 `src/api/routes_metrics.rs`(handler + 文本格式化 render 函数)
  - 改 `src/api/mod.rs`(`build_router` 注册 `GET /metrics` + `PUBLIC_PATHS` 加白名单)
  - 改 `src/process_metrics.rs`(新增 sysinfo 采样公共函数,见 §7)
  - 改 `src/api/routes_sys.rs`(`/sysstatus` 改用公共函数,删内联 sysinfo)

## 4. 指标清单(完整命名映射)

Prometheus 命名前缀统一 `rs_iot_`。counter 必带 `_total` 后缀,gauge 无后缀,info metric 带 `_info`。

### 4.1 进程标识 + 资源

| metric | type | HELP | 来源 / 说明 |
|---|---|---|---|
| `rs_iot_info{hwid}` | gauge(info) | rs-iot 进程标识(版本/hwid) | `{hwid="eiot_xxxx",version="x.y"} 1`。多机区分靠 Prometheus `instance` 标签,hwid 仅作业务自标识 |
| `rs_iot_uptime_seconds` | gauge | 进程运行秒数 | `ProcessMetrics.started.elapsed().as_secs()` |
| `rs_iot_process_cpu_pct` | gauge | 进程 CPU 占用百分比 | sysinfo `process(pid).cpu_usage()`(0.0-100.0) |
| `rs_iot_process_mem_bytes` | gauge | 进程常驻内存字节 | sysinfo `process(pid).memory()` |

### 4.2 控制下行(CtlRegistry)

| metric | type | 来源(M4 atomic) |
|---|---|---|
| `rs_iot_ctl_send_total` | counter | `ctl_send_count` |
| `rs_iot_ctl_dropped_total` | counter | `ctl_dropped_count` |
| `rs_iot_ctl_not_found_total` | counter | `ctl_not_found_count` |

### 4.3 数据上行(pipeline)

| metric | type | 来源 |
|---|---|---|
| `rs_iot_pipeline_processed_total` | counter | `pipeline_processed_count` |
| `rs_iot_pipeline_write_fail_total` | counter | `pipeline_write_fail_count` |

### 4.4 hub 背压

| metric | type | 来源 |
|---|---|---|
| `rs_iot_hub_depth` | gauge | `hub_depth_last`(当前深度) |
| `rs_iot_hub_depth_max` | gauge | `hub_depth_max`(历史峰值) |

### 4.5 北向(mqtt)

| metric | type | 来源 |
|---|---|---|
| `rs_iot_mqtt_publish_total` | counter | `mqtt_publish_count` |
| `rs_iot_mqtt_publish_fail_total` | counter | `mqtt_publish_fail_count` |
| `rs_iot_mqtt_connected` | gauge | `mqtt_connected`(AtomicBool → 0/1) |

### 4.6 per-instance(带 `{inst_id}` label)

per-instance 指标遍历 `InstanceManager::status()`(进程级 `InstState`,实例列表)+
lux `stat:<inst_id>`(插件级 `InstanceStat`)。进程级 `state`(死活)与插件级 `status`(健康度)
合并进单个 info metric 的两个 label。

| metric | type | labels | 来源 |
|---|---|---|---|
| `rs_iot_instance_info` | gauge(info) | `inst_id`, `app_code`, `host_kind`, `state`, `health` | `mgr.status()` 给 `state` + 分类;`InstanceStat.status` 给 `health`;值恒 1 |
| `rs_iot_instance_tick_total` | counter | `inst_id` | `InstanceStat.tick_count` |
| `rs_iot_instance_fail_total` | counter | `inst_id` | `InstanceStat.fail_count` |
| `rs_iot_instance_last_activity_duration_ms` | gauge | `inst_id` | `InstanceStat.last_activity_duration_ms`(u64 毫秒) |
| `rs_iot_instance_last_activity_ts` | gauge | `inst_id` | `InstanceStat.last_activity_ts`(i64 unix 秒;Grafana 算 `now()-ts` 判活跃) |

**label 受控词汇**(固定词汇,便于 Grafana 过滤 / Alertmanager 规则匹配):
- `state`(进程级 `InstState`,`#[serde(rename_all="lowercase")]`):`starting` / `running` / `stopping` / `stopped` / `failed`
- `health`(插件级 `InstanceStat.status`):`running` / `degraded` / `stopped`
- `host_kind`:`builtin` / `external`(与 `/inst/stats` 的 `host_kind_str` 一致)
- `app_code`:实例 `app_code`(如 `opcuaClient`/`modbusClient`/`luaClient`/`sidecarClient`),开放词汇(随插件扩展)
- 实例刚启动尚未 flush stat 时 `health` 为空字符串(`health=""`,Prometheus 合法)

**不暴露**:`InstanceStat.last_err`(自由文本,不适合 metric;走 `/inst/stats` JSON / 日志)、
`InstanceStat.extra`(动态 flatten,命名不可控;后置)。

## 5. 命名约定

- counter:`_total` 后缀(`ctl_send_count` → `rs_iot_ctl_send_total`)。Prometheus scrape 时自动衍生 `_created` 可选(本实现不输出 `_created`,非必需)
- gauge:无后缀(`hub_depth`、`uptime_seconds`)
- 耗时:`_ms` 后缀;时间戳:`_ts`(unix 秒);字节:`_bytes`;百分比:`_pct`;秒:`_seconds`
- info metric:`_info` 后缀,值恒 1,所有分类/标识走 label
- label 值含特殊字符(`"`/`\`/换行)须 escape(`\"`/`\\`/`\n`);inst_id/app_code 一般安全,但 render 须做 escape 兜底

## 6. 数据流

handler(`routes_metrics::metrics`)顺序:
1. `AppState.metrics` atomic `load(Relaxed)` → 进程级计数(ctl/pipeline/hub/mqtt)
2. `ProcessMetrics.started.elapsed()` → uptime
3. sysinfo 采样 → cpu/mem(公共函数,见 §7)
4. `mgr.status()` → 实例列表 + 进程级 `state`(HashMap<inst_id, InstState>)
5. 每实例:`mgr.config_snapshot(inst_id)` → `app_code`/`host_kind`;lux `GET stat:<inst>` → `InstanceStat`(health + 计数)。无 stat 的实例(degraded 且未 flush / 刚启动)info metric 仍输出(`health` label 空)
6. 拼接 Prometheus 文本,`Content-Type: text/plain; version=0.0.4; charset=utf-8`,200 返回

**per-instance lux 读**:N 个实例 = N 次 `GET stat:<inst>`(沿用 `/inst/stats` 模式)。
N 通常几十,可接受。不引入 lux `SCAN stat:*`(复杂且 stat key 与其它 key 混在默认 db)。

**失败降级**:任一 lux 读失败 → 该实例跳过 per-instance 计数,但进程级指标照常输出(单实例 stat 读失败不应让整个 `/metrics` 500)。

## 7. sysinfo 采样公共化(DRY 重构)

当前 sysinfo 采样内联在 `routes_sys.rs:sysstatus`(M4 Task 17)。`/metrics` 也要用,抽公共函数避免复制:

- 新增 `process_metrics::sample_process_resource() -> (f32, u64)`(cpu_pct, mem_bytes)
  - 内部:`System::new_all()` + `get_current_pid().unwrap_or(Pid::from(0_usize))` + `process(pid).map(|p| (p.cpu_usage(), p.memory())).unwrap_or((0.0, 0))`
- `routes_sys.rs:sysstatus` 改调该函数(删内联 sysinfo 块)
- `routes_metrics.rs:metrics` 调同一函数

放 `process_metrics.rs` 而非新模块——它已是「进程级指标」的归属(`ProcessMetrics` 在此),sysinfo 资源采样语义同源。

## 8. 安全边界

- `/metrics` 免 JWT:暴露内容仅计数器/状态/资源指标,**无凭据/无业务数据值**(实时采集值在 lux `rtdata:<dev>`,不在 `/metrics`)。lux 本就 loopback bind,工业内网信任假设下可接受
- `hwid` 出现在 `rs_iot_info` label——CLAUDE.md 允许 hwid 在日志/API 出现
- 不暴露 `last_err` 文本(可能含 endpoint 片段等,且非 metric 语义)
- 跨网部署(暴露 rs-iot 到非信任网络)时,必须加 IP 白名单/mTLS 或反代鉴权——本 spec 不做,PoC 内网假设

## 9. 测试策略

- **单测**(render 函数,`routes_metrics.rs` 内 `#[cfg(test)]`):
  - 给定 mock `ProcessMetrics`(atomic 预置值)+ 构造的 per-instance 数据 → 调 render → 断言输出含:
    - 每个 counter 有 `# TYPE ... counter` + `_total` 后缀
    - 每个 gauge 有 `# TYPE ... gauge`
    - `# HELP` 行存在
    - label 格式正确(`metric{label="value"} number`)
    - label escape(`"`/`\` 被转义)
    - `rs_iot_info{hwid=...}` 含 hwid
- **集成测**(`tests/metrics_e2e.rs`):
  - 起 lux + axum(`tests/common::start_lux`,端口 0 + 临时 data_dir)
  - 预置一个实例 stat(写 lux `stat:<inst>`)
  - `GET /metrics`(**不带 JWT**)→ 200 + `content-type` 含 `text/plain` + body 含 `rs_iot_` 前缀 + 含 per-instance metric 行
  - 断言进程级 counter 出现(`rs_iot_ctl_send_total` 等)
- **命名规范锁定**:单测显式断言所有 counter 名以 `_total` 结尾、无 gauge 误带 `_total`

## 10. 与 M4 的关系

- **数据源 = M4 已埋**:`ProcessMetrics`(进程级 atomic)+ `InstanceStat`(per-instance lux)+ sysinfo 资源(M4 Task 17)。`/metrics` 纯输出格式化,零新埋点
- **M4 前瞻命名的兑现**:M4 spec 定的 `_count`/`_ms`/`_bytes` → 本 spec 映射为 counter `_total` / gauge `_ms`/`_bytes`,无重命名负担
- **`/sysstatus` 不变**:JSON REST 保留(人查场景),`/metrics` 是并行的机器出口。sysinfo 采样抽公共后两者复用,行为不变

## 11. 后续

- **P1 子系统 2**:报文 trace 开关(per-instance 运行时激活收发字节 hex dump)——独立 spec
- **P1 子系统 3**:日志 ring buffer(tracing layer 截获最近 N 条 + `GET /api/v1/logs/recent`)——独立 spec。
  报文 trace P1 走「dump 进 tracing 日志」时,日志 ring buffer 天然捕获,两者经日志媒介衔接
- **extra 标准化**:若现场需要 lua `vm_restart_count` / opcua 重连计数等进 Prometheus,提升为 `InstanceStat` 核心 field 或加专用 per-instance metric(后续按需)
- **histogram**:若需采集耗时分布(非仅 last),后置加 summary/histogram
