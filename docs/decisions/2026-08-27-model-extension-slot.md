# 决策：设备模型开放扩展槽（DeviceModel/TagTemplate.extra）与键契约

> **状态**：implemented（扩展槽 + `x_` 键契约 + 读透出面同批落地）
> **实施**：2026-08-27 并入（extra 扩展槽 + x_ 键契约）
> **日期**：2026-08-27
> **实施**：2026-08-27 `3c59989` 扩展槽 + `x_` 键契约 + 读工具/前端展示
> **设计**：[2026-08-27-model-extension-slot-design.md](../designs/2026-08-27-model-extension-slot-design.md)（完整论证与规范性小节）
> **代码**：`src/model/types.rs`（flatten extra 槽）、`src/model/validate.rs`（键校验）、`src/model/store.rs`（写入口把关）、`crates/rsiot-mcp`（读工具）、`frontend/src/lib/devmeta.ts` + `ModelForm.vue`（只读展示）
> **前置**：[2026-08-25-device-model-registry.md](2026-08-25-device-model-registry.md)（D-M1/D-M2 元数据唯一出生地）

## 背景与冲突

用户构想：DeviceModel/TagTemplate 作为工业时序数据应用的基础资产层，须能附加厂商资料/固件、厂商方法逻辑、报警/清洗/存储策略等元数据，供外置应用获取处理。现状两个缺口：

1. 结构体封闭，协议私有元数据被挤进 addr/data_type 字符串；
2. 模型读写为「反序列化→整体序列化覆盖」，serde 丢弃未知字段——**版本偏差下旧二进制读改写会静默丢失新字段**（红测实证）。

## 决策（三项）

**D-E1 开放扩展槽**：`DeviceModel.extra` / `TagTemplate.extra`，`#[serde(flatten, skip_serializing_if = "Map::is_empty")]`——未知键捕获保留、空槽 wire 逐字节不变、读改写无损。宿主**不解释**任何扩展键（信封非配置）。

**D-E2 键契约 fail-closed**：扩展键 MUST `^x_[A-Za-z0-9_]+$`；`validate_model` 校验，store 两写入口把关，REST 违规 400。前缀使与已知字段碰撞结构性不可能，typo 键不再无声持久化。

**D-E3 定位边界**：扩展性是设计预案非功能义务——报警等属外置应用（宿主只留事件通道），存储 10w tag 内自持、规模大了外推退出；每一项延后能力（extra 覆盖/报警契约/脚本绑定/大资产）等第一个真实消费者出现再动工。

## 风险登记

- REST 请求体未知键从"静默丢弃"变为"捕获持久化回显"——行为变化，由键契约将 typo 拒之门外；侧写错误（合法 `x_` 前缀但语义错）不设防，由消费方自责。
- flatten 反序列化走 serde Content 缓冲有轻微开销（配置级小对象，忽略不计）。
