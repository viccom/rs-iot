# 插件扩展策略评估：动态库 vs SDK crate vs sidecar

> **状态**：评估完成 / 决策记录（ADR 性质）
> **日期**：2026-08-07
> **相关代码**：`src/plugin/traits.rs`、`src/plugin/registry.rs`、`src/plugin/instance.rs`、`src/host.rs`、`src/plugin/builtin/`
> **相关文档**：`docs/external-plugin-protocol.md`（sidecar 协议）、`docs/lua-script-guide.md`（Lua 脚本）

## 1. 背景与诉求演进

rs-iot 当前内置插件与核心代码**一起编译为单一二进制**（`registry.rs` 基于 `inventory` 编译期静态注册）。讨论起因于一个设想：能否把内置插件改为**动态库**形式，实现自动重载注册。

经过两轮讨论，真实诉求逐步澄清为三层（按重要性）：

1. **团队协作**：避免每个开发者都拉全量代码库（lux + 全部协议栈）编译。新协议开发者应在隔离的、小的、可独立编译的单元里工作。
2. **第三方 / 客户生态**：定义稳定的宿主—插件接口，客户能在 AI 加持下自行开发插件，逐步形成 rs-iot 生态。
3. **现场裁剪**：不同项目、不同现场需要的采集插件不同，不必每个部署都背全量功能，降低资源占用与系统依赖。

**关键澄清**：上述三层诉求中，"动态库"只占很小一块——它解决的仅是"客户插件**分发**时不必重编主程序"。而"开发体验"与"生态契约"有更高性价比的解法（见第 6 节）。

## 2. 技术约束：Rust 动态库的硬骨头

Rust **没有稳定 ABI**。把当前插件契约原样塞进 `.dll`/`.so` 不可行。当前契约（`traits.rs:16-29`）：

```rust
#[async_trait]
pub trait Source: Send + Sync {
    fn name(&self) -> &'static str;
    async fn run(
        &self,
        ctx: HostContext,                  // lux::EmbeddedClient + CancellationToken + DashMap
        inst: InstConfig,                   // 含 serde_json::Value 任意 JSON
        tx: mpsc::Sender<Sample>,           // tokio mpsc
        cmd_rx: mpsc::Receiver<CtlMessage>, // tokio mpsc
        token: CancellationToken,           // tokio-util
    );
}
```

工厂注册侧（`registry.rs:13`）：`create: fn() -> Box<dyn Source>` —— 函数指针 + trait object（vtable）。

跨动态库边界时，5 颗炸弹同时引爆：

| # | 跨边界对象 | 为什么炸 |
|---|---|---|
| 🔴1 | `Box<dyn Source>` 的 **vtable** | 虚表布局由 rustc 内部决定，主程序与插件库必须**同 rustc 版本 + 同依赖版本 + 同 feature + 同 panic 策略**编译，否则 vtable 错位 = 未定义行为。这是语言既定事实，非配置可解。 |
| 🔴2 | **async trait 跨 FFI** | `#[async_trait]` 把 `run` 展开为 `fn run(...) -> Pin<Box<dyn Future<Output=()> + Send>>`。`Pin<Box<dyn Future>>` 是纯 Rust 类型，**无法跨 `extern "C"`**。必须重设计为同步 poll/step 或自定义 C ABI future 协议。 |
| 🔴3 | **`HostContext` by value** | `host.rs:124` 含 `lux::EmbeddedClient`（lux 内部类型）、`DashMap<String, mpsc::Sender<_>>`。每个字段布局都绑定具体编译产物。按值跨库传递 = 直接 UB。 |
| 🟠4 | **`InstConfig`/`Sample`/`CtlMessage`/`RsIotError`** | 自定义类型；`RsIotError` 是 `#[non_exhaustive]`——跨库 match 它本身不安全。 |
| 🟠5 | **tokio runtime 归属** | 当前 `tokio::spawn` 在主进程（`instance.rs:94`），插件 future 跑主 runtime——这点对动态库**有利**（插件不必自带 runtime），但插件库来的 future 跨库 spawn 的 panic/Send 边界仍需处理。 |

**结论**：不能直接复用 `Source` trait。必须在主程序侧插入一层 **C ABI 适配层**（手写 vtable 或借助 `abi_stable` crate），把所有 Rust 类型在边界拆解成 C 兼容表示。这是动态库方案的**固定成本**，与是否热加载无关。

## 3. 热重载 vs 重启加载：风险对比

"自动重载"（unload 旧库 + load 新库）的难点不在 load，在 **unload**。

| 风险点 | 自动热加载 | 重启加载 |
|---|---|---|
| 🔴 unload 时序 segfault（task 持 `Box<dyn Source>` 时 `dlclose`） | 存在 | **消除** |
| 🔴 Windows DLL 卸载难题（引用计数 + TLS + 信号处理敏感） | 存在 | **消除** |
| 🟠 引用计数 / drain 协议 | 要做 | 不需要 |
| 🔴 vtable ABI（插件须同工具链编译） | 存在 | **仍在** |
| 🔴 async trait 跨 FFI | 存在 | **仍在** |
| 🔴 `HostContext` 不能 by-value 跨边界 | 存在 | **仍在** |
| 🟠 插件 panic 崩主进程 | 存在 | **仍在** |

**重启加载**消除了最致命的 unload 类风险，把实现成本从约"6–12 周"降到约"3–6 周"，但**绕不过 C ABI 适配层的固定门槛**。运行期插件 panic 崩主进程的风险依然存在——对工业网关 7×24 场景，这是风险量级的跃升。

## 4. 收益的诚实审视

### 4.1 "降低资源占用" —— 部分成立但被高估

静态二进制里，未使用的插件经 LTO（`Cargo.toml:108` 已开 `lto="thin"` + `strip`）+ dead code elimination，**未实例化的插件运行时几乎零开销**。工业网关内存通常 GB 级，省代码段几 MB 意义有限。

真实收益在**系统依赖**：`tokio-serial`（Linux 需 `libudev-dev`）、`async-opcua`（密码学栈）。现场不跑该协议时可不装这些系统库——这是部署层真收益。真正省的是**二进制体积**（OTA 包变小）和**编译时间**，不是常驻内存。

> 注：现场裁剪 + 系统依赖可选的收益，可由 **Cargo feature 裁剪**以极低成本获得，不必动态库。见 `docs/plan-feature-gating-protocols.md`。

### 4.2 "自动重载 + 灵活组合" —— 方向对，但 rs-iot 已有两条路覆盖

| 已有能力 | 落地状态 | 覆盖的诉求 |
|---|---|---|
| **Lua 脚本（M8b）** | ✅ 已落地，有 `hotload.rs` 热加载 | 小点数设备轻量采集、自定义协议 |
| **sidecar 外部插件（M7）** | ✅ 已落地，有 `tests/sidecar_e2e.rs` + Python/Go 样例 | 各行各业标准协议外挂、跨语言、进程隔离 |

动态库方案要和这两条**已落地、零 ABI 风险**的路径直接比 ROI。Lua + sidecar 已实现"灵活 + 解耦 + 可热更新"，动态库是第三条路，**收益边际、风险最高**。

### 4.3 "AI 加持客户自开发" 的现实判断

- **AI 写 sidecar 插件（Python/Go）**：协议明文、有 `examples/ext-plugin-python/` 与 `examples/ext-plugin-opcda-go/` 样例参照，AI 复制改写成功率**高**。
- **AI 写 Rust 动态库插件**：要同时处理 C ABI、`extern "C"` 回调、async 边界、`unsafe` 指针转换、跨边界 panic——恰是 AI **最易生成隐性 UB** 的区域。而 UB 插件一旦崩溃，带倒的是 rs-iot 主进程，现场事故算在 rs-iot 头上。

**动态库方案会把客户插件的质量风险，转嫁为 rs-iot 主进程的稳定性风险**——对"形成生态"是反效果。

## 5. 方案矩阵

| 方案 | 灵活组合 | 热更新 | 内存/部署收益 | 稳定性 | 成本 | 现状 |
|---|---|---|---|---|---|---|
| **A. 现状（全静态）** | ✗ | ✗ | ✗ | ★★★★★ | 0 | 已有 |
| **B. Cargo feature 裁剪** | 部分（编译期） | ✗ | ✓ 体积/依赖 | ★★★★★ | **极低（1–2 天）** | transform/lua 已用；opcua/modbus/s7 未做 |
| **C. Lua 脚本（M8b）** | ✓ | ✓ | ✓ 轻量 | ★★★★ | 已落地 | 已有 |
| **D. sidecar 外部进程（M7）** | ✓✓（跨语言） | ✓✓ | ✓ 隔离 | ★★★★★ | 已落地 | 已有 |
| **E. 动态库自动重载（原始提案）** | ✓✓ | ✓✓ | ✓ 体积/依赖 | ★★（crash+ABI） | 6–12 周 + 长期维护 | 无 |
| **E'. 动态库重启加载（折中）** | ✓✓ | ✓（重启） | ✓ 体积/依赖 | ★★★（无 unload 风险） | 3–6 周 + 长期维护 | 无 |

## 6. 关键洞察：诉求的真正解法

三层诉求与最优解的映射：

| 诉求 | 最优解 | 动态库是否必需 |
|---|---|---|
| 团队开发不用拉全量代码 | **抽 `rs-iot-plugin-sdk` crate**（trait + 类型抽成独立小 crate） | 否 |
| 客户自行开发插件形成生态 | **SDK crate（契约）+ sidecar（分发主力）** | 否 |
| 现场裁剪 + 系统依赖可选 | **Cargo feature 裁剪（方案 B）** | 否 |

**动态库在三条诉求里都不是必需**。最高杠杆的动作是：
1. 抽 `rs-iot-plugin-sdk` crate —— 生态契约基石（1–2 周，纯收益）
2. Cargo feature 裁剪 —— 现场裁剪（1–2 天，见 `docs/plan-feature-gating-protocols.md`）
3. 深化 sidecar —— 客户生态主力通路（已落地，投入在文档/样例/SDK）

## 7. 推荐分层策略

```
第一层（必做）：抽 rs-iot-plugin-sdk crate
  → 解决团队/客户开发体验，生态契约基石，对静态/sidecar 都有益

第二层（主力分发）：
  ├─ 内置标准协议（opcua/modbus/s7）：sdk + 静态链接 + Cargo feature 裁剪
  │   → 最稳，团队维护，现场按需编译
  └─ 第三方/客户生态：主推 sidecar（已落地）
      → 任意语言、进程隔离、AI 友好、已有样例

第三层（可选，小众）：动态库重启加载
  → 仅留给"必须进程内 + 必须 Rust + 性能敏感"的窄场景
  → 需投入 C ABI 适配层（3-6 周），作为 sdk 之上的可选通路
  → 能力 ≤ sidecar，风险 ≥ sidecar
```

## 8. 若将来要做动态库，最小可行路径

仅当未来撞上"sidecar IPC 开销不可接受 + 必须 hot reload + 必须进程内"三重约束时推进：

1. **先抽 SDK crate**（省不掉，对静态/sidecar 都有益）
2. **不复用 `Source` trait**：在 SDK 另定义 `ExternalSource` C ABI 接口（借助 `abi_stable` 锁 ABI），插件实现 C 接口，主程序写 adapter 包成 `Source`
3. **重启加载**：启动时扫描 `plugins/` 目录，`libloading` 加载注册，**不做 unload**
4. **严格工具链约定**：插件须用 rs-iot 发布的"插件工具链 + SDK 版本"编译（文档 + 启动时版本字符串校验）
5. **能力受限**：动态库插件拿不到 `EmbeddedClient` 零拷贝，只能走"上报回调 → 主程序序列化"，本质是 sidecar 的进程内变体

## 9. 结论

- **动态库（无论热加载还是重启加载）不是当前阶段的最优投入**：诉求 90% 由 SDK crate + sidecar + Cargo feature 满足，且零 ABI 风险。
- **立即推进**：方案 B（Cargo feature 裁剪，`docs/plan-feature-gating-protocols.md`）—— 现场裁剪的最高性价比解法。
- **中期推进**：抽 `rs-iot-plugin-sdk` crate —— 生态契约基石。
- **持续深化**：sidecar 文档/样例/SDK —— 客户生态主力通路。
- **动态库**：留作未来确有进程内 hot-reload 硬需求时的选项，定位第三层小众补充。
