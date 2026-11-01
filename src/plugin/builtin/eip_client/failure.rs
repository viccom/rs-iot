//! 失败归属与批量预算：把库返回的错误分成"传输层 / 批量过大 / 类型不支持 / 点级"，
//! 驱动"拆连接重建 / 减小批量重试 / 跳过并告警 / 仅该点失败"四种处置。
//!
//! 台账（`docs/designs/2026-11-01-protocol-semantics-audit.md` §6）对应关系：
//! - **E-1** 批量组级失败被库降级成点级 `NetworkError`：必须按消息文本重新分类，
//!   否则真断链被当成"单点失败"只 warn → 毒化流后永久停采；
//! - **E-11** 批量只按请求字节打包、`0x06 Partial Transfer` 不重组：插件侧按字节预算
//!   切包 + 半拆重试一次；
//! - **E-12** 封装层状态码（0x64/0x65/0x69）被库映射成非 retriable 的 `Protocol`：
//!   插件按连接失效处理（重建客户端）；
//! - **E-19** 库打包长度少算 16B SendRRData/CPF 固定头：估算里补上。
//!
//! 上游证据根 `<REG>` = `/root/.cargo/registry/src/rsproxy.cn-e3de039b2554c837/`。

use rust_ethernet_ip::{BatchError, EtherNetIpError};

/// 一次读失败的处置类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FailureClass {
    /// 传输层失效：连接已不可用（含 E-12 封装状态码），必须拆连接重建。
    Transport,
    /// 批量本身过大/被拒（`0x06 Partial Transfer`、MSP 打包错误）：减小批量重试一次。
    BatchTooLarge,
    /// 类型码在库解码层无分支（E-3）：该点跳过（Null）+ 只告警一次。
    UnsupportedType,
    /// 点级失败（tag 不存在 / 类型不符 / 单点 CIP 状态码）：仅该点失败。
    Point,
}

/// 单点路径错误分类（`read_tag` / `read_string_tag` / 写 / 健康探针共用）。
pub(super) fn classify_error(e: &EtherNetIpError) -> FailureClass {
    // 库自述的可重试集（Timeout/Connection/ConnectionLost/Io，error.rs:102-114）→ 拆连接。
    if e.is_retriable() {
        return FailureClass::Transport;
    }
    match e {
        EtherNetIpError::Protocol(msg) => classify_protocol_msg(msg),
        _ => FailureClass::Point,
    }
}

/// `EtherNetIpError::Protocol` 文本细分。
///
/// 该变体被库塞了三类语义完全不同的东西：
/// 1. 封装层状态码 → 会话失效（E-12）；
/// 2. 类型码无解码分支 → 该点永久无值（E-3）；
/// 3. 其余（tag 路径编码错、应答解析错、MSP 打包错）→ 点级或批量过大。
fn classify_protocol_msg(msg: &str) -> FailureClass {
    if msg.contains("EIP Command failed. Status: 0x") {
        // <REG>rust-ethernet-ip-1.2.0/src/client.rs:3823-3841：封装状态非 0 走 Protocol，
        // 库只在成功响应时清 stream_poisoned → 同会话下拍必然再失败，必须重建。
        // 台账点名的 0x64（无效会话句柄）/0x65（无效长度）/0x69（协议版本不支持）之外，
        // 任何非 0 封装状态都意味着会话/协议面不可用（需重新 RegisterSession），一并重建。
        return FailureClass::Transport;
    }
    if msg.contains("Unsupported data type") {
        return FailureClass::UnsupportedType;
    }
    if msg.contains("Multiple Service") || msg.contains("Reply count mismatch") {
        // MSP 级错误：整包太大/应答不匹配 → 减小批量重试（E-11）。
        return FailureClass::BatchTooLarge;
    }
    FailureClass::Point
}

/// 批量路径单点错误分类（`read_tags_batch` 的 per-op `BatchError`）。
pub(super) fn classify_batch_error(e: &BatchError) -> FailureClass {
    match e {
        // E-1：库把"整组失败"逐个 op 包成 NetworkError(Display(原始错误))
        // （<REG>rust-ethernet-ip-1.2.0/src/client/batch_exec.rs:106-125），
        // 原始错误类型只能从文本复原。
        BatchError::NetworkError(msg) => classify_network_error_msg(msg),
        // 包级超时 = 无应答（库 batch packet_timeout_ms）。
        BatchError::Timeout => FailureClass::Transport,
        BatchError::CipError { status, .. } => classify_cip_status(*status),
        BatchError::SerializationError(msg) if msg.contains("Unsupported data type") => {
            FailureClass::UnsupportedType
        }
        // 应答结果数少于请求数（batch_exec.rs:497-508）：打包/解析错位，拆小重试。
        BatchError::Other(msg) if msg.contains("Missing result") => FailureClass::BatchTooLarge,
        _ => FailureClass::Point,
    }
}

/// 单条 CIP 服务状态码分类。
///
/// `0x06 Partial Transfer`（E-11）、`0x15 Too much data`、`0x1A/0x1B 路由包过大`
/// 都与"一条消息装不下"有关 → 减小批量重试；其余状态码（0x04 路径错、0x05 目标不存在、
/// 0x08 服务不支持、0x0F 权限…）是点级语义。
fn classify_cip_status(status: u8) -> FailureClass {
    match status {
        0x06 | 0x15 | 0x1A | 0x1B => FailureClass::BatchTooLarge,
        _ => FailureClass::Point,
    }
}

/// `BatchError::NetworkError` 文本 → 类别（E-1 的核心）。
fn classify_network_error_msg(msg: &str) -> FailureClass {
    if msg.contains("EIP Command failed. Status: 0x") {
        return FailureClass::Transport; // E-12
    }
    if msg.contains("Unsupported data type") {
        return FailureClass::UnsupportedType; // E-3（组内解码失败）
    }
    // EtherNetIpError 的 Display 前缀（error.rs:12-100）。
    if msg.starts_with("IO error")
        || msg.starts_with("Operation timed out")
        || msg.starts_with("Connection error")
        || msg.starts_with("Connection lost")
    {
        return FailureClass::Transport;
    }
    if msg.starts_with("Protocol error") {
        return classify_protocol_msg(msg);
    }
    // 库只在这条分支包装"整组失败"；未知文本保守按传输层处理（重建可恢复）。
    FailureClass::Transport
}

/// 每个 Read Tag 子请求的保守在线字节估算（E-19）。
///
/// service(1) + path_size(1) + ANSI 扩展符号段(2 + len) + 奇偶填充(≤1) + count(2)。
/// 库的打包只按请求侧估且**少算 16B** SendRRData/CPF 固定头
/// （<REG>rust-ethernet-ip-1.2.0/src/client/batch_exec.rs:422-457 对比 client.rs:3641-3698），
/// 这里按"多估不少估"取 8 + len（含 1B 填充余量）。
pub(super) fn estimate_read_op_bytes(tag: &str) -> usize {
    8 + tag.len()
}

/// 一个 MSP 包除了各 op 请求体之外的固定开销（E-19）。
///
/// 16B（SendRRData/CPF 固定头，库打包时**少算**）+ 12B（Unconnected Send 头）
/// + 8B（MSP 头：service+path+服务数）。每 op 另加 2B 偏移表。
pub(super) const BATCH_FIXED_OVERHEAD: usize = 36;

/// 按字节预算贪心切包（E-11）。
///
/// 单 tag 自身超预算时独占一包（否则会死循环/丢点）；调用方拿到单包仍失败就降到
/// 逐 tag 读。
pub(super) fn chunk_tags_by_bytes<'a>(tags: &[&'a str], max_bytes: usize) -> Vec<Vec<&'a str>> {
    let max_bytes = max_bytes.max(1);
    let mut chunks: Vec<Vec<&'a str>> = Vec::new();
    let mut current: Vec<&'a str> = Vec::new();
    let mut est = BATCH_FIXED_OVERHEAD;
    for tag in tags {
        let op = 2 + estimate_read_op_bytes(tag);
        if !current.is_empty() && est + op > max_bytes {
            chunks.push(std::mem::take(&mut current));
            est = BATCH_FIXED_OVERHEAD;
        }
        current.push(*tag);
        est += op;
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// E-1/E-12：传输层错误（含封装状态码 Protocol）必须判为 Transport。
    #[test]
    fn transport_errors_are_transport() {
        for e in [
            EtherNetIpError::Timeout(Duration::from_secs(10)),
            EtherNetIpError::Connection("refused".into()),
            EtherNetIpError::ConnectionLost("reset".into()),
            EtherNetIpError::Io(std::io::Error::other("broken pipe")),
            // E-12：0x64 无效会话句柄 / 0x65 无效长度 / 0x69 版本不支持
            EtherNetIpError::Protocol("EIP Command failed. Status: 0x00000064".into()),
            EtherNetIpError::Protocol("EIP Command failed. Status: 0x00000069".into()),
        ] {
            assert_eq!(classify_error(&e), FailureClass::Transport, "{e}");
        }
    }

    /// E-1：点级错误不得触发拆连接（视为回环风暴的来源）。
    #[test]
    fn point_errors_stay_point() {
        for e in [
            EtherNetIpError::TagNotFound("Nope".into()),
            EtherNetIpError::DataTypeMismatch {
                expected: "BOOL or DINT".into(),
                actual: "Int(1)".into(),
            },
            EtherNetIpError::Protocol("Protocol error: Expected identifier".into()),
            EtherNetIpError::CipError {
                code: 0x04,
                message: "Path segment error".into(),
            },
        ] {
            assert_eq!(classify_error(&e), FailureClass::Point, "{e}");
        }
    }

    /// E-3：读回类型无解码分支 → UnsupportedType（跳过 + 只告警一次）。
    #[test]
    fn unsupported_readback_type_detected() {
        let e = EtherNetIpError::Protocol("Protocol error: Unsupported data type: 0x00D2".into());
        assert_eq!(classify_error(&e), FailureClass::UnsupportedType);
        let b = BatchError::SerializationError("Unsupported data type: 0x00D4".into());
        assert_eq!(classify_batch_error(&b), FailureClass::UnsupportedType);
    }

    /// E-1：库把组级失败包装成点级 NetworkError——文本必须能复原成传输层。
    #[test]
    fn batch_network_error_is_reclassified_by_message() {
        assert_eq!(
            classify_batch_error(&BatchError::NetworkError(
                "Connection lost: connection reset by peer".into()
            )),
            FailureClass::Transport
        );
        assert_eq!(
            classify_batch_error(&BatchError::NetworkError(
                "Operation timed out after 10s".into()
            )),
            FailureClass::Transport
        );
        assert_eq!(
            classify_batch_error(&BatchError::NetworkError(
                "EIP Command failed. Status: 0x00000065".into()
            )),
            FailureClass::Transport,
            "E-12 封装状态码混进批量路径也要拆连接"
        );
        assert_eq!(
            classify_batch_error(&BatchError::NetworkError(
                "Protocol error: Multiple Service Response error: 0x1F".into()
            )),
            FailureClass::BatchTooLarge,
            "MSP 组级协议错误 → 拆小重试，不拆连接"
        );
    }

    /// E-11：0x06 Partial Transfer / 包级过大 → 减小批量重试一次。
    #[test]
    fn partial_transfer_asks_smaller_batch() {
        for status in [0x06u8, 0x15, 0x1A, 0x1B] {
            assert_eq!(
                classify_batch_error(&BatchError::CipError {
                    status,
                    message: "x".into()
                }),
                FailureClass::BatchTooLarge,
                "status 0x{status:02X}"
            );
        }
        assert_eq!(
            classify_batch_error(&BatchError::CipError {
                status: 0x04,
                message: "Path segment error".into()
            }),
            FailureClass::Point,
            "路径错误是点级"
        );
        assert_eq!(
            classify_batch_error(&BatchError::Other("Missing result from response".into())),
            FailureClass::BatchTooLarge
        );
    }

    /// E-11/E-19：按字节预算切包——不丢 tag、每包估算不超预算（单 tag 超预算除外）。
    #[test]
    fn chunking_respects_byte_budget() {
        let names: Vec<String> = (0..60)
            .map(|i| format!("Program:Main.Tag_{i:03}"))
            .collect();
        let tags: Vec<&str> = names.iter().map(String::as_str).collect();
        let chunks = chunk_tags_by_bytes(&tags, 200);
        assert!(
            chunks.len() > 1,
            "20B tag × 60 应切成多包: {}",
            chunks.len()
        );
        assert_eq!(
            chunks.iter().map(Vec::len).sum::<usize>(),
            tags.len(),
            "切包不得丢点"
        );
        for c in &chunks {
            let est = BATCH_FIXED_OVERHEAD
                + c.iter()
                    .map(|t| 2 + estimate_read_op_bytes(t))
                    .sum::<usize>();
            assert!(
                est <= 200 || c.len() == 1,
                "包估算 {est}B 超预算且不止一个点"
            );
        }
        // 单个超预算 tag 独占一包（不死循环、不丢点）
        let long = "A".repeat(500);
        let one = chunk_tags_by_bytes(&[long.as_str()], 200);
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].len(), 1);
        // 空输入
        assert!(chunk_tags_by_bytes(&[], 200).is_empty());
    }

    /// E-19：估算包含 16B 固定头余量（16+12+8 = 36B 空包开销 + 2B/op 偏移表）。
    #[test]
    fn wire_estimate_includes_fixed_overhead() {
        assert_eq!(BATCH_FIXED_OVERHEAD, 16 + 12 + 8);
        assert_eq!(estimate_read_op_bytes("Tag"), 11);
    }
}
