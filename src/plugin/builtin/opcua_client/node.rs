//! 点位寻址：NodeId 语法扩展（OPC-8）、数组下标范围（OPC-15）、写目标解析（F-1）。
//!
//! 库的 `NodeId::from_str` 只认 `ns=<数字>;i|s|g|b=`（小写、无空白），不支持
//! `nsu=<URI>`（要经服务端 NamespaceArray 换成索引）。这里在库之上做一层解析：
//! - 前缀大小写不敏感、允许前后空白；
//! - `nsu=urn:xxx;s=Name` 用连接后读到的 [`NamespaceMap`] 解析索引；
//! - 末尾 `[n]` / `[a:b]` 解析成 `NumericRange`（读写共用的 index_range）。
//!
//! 解析失败一律 `Err` 且错误文本带**原始串**（现场配置排障的第一现场证据）。

use opcua::types::{Identifier, NamespaceMap, NodeId, NumericRange};

/// 一个点位地址：NodeId + 可选的数组下标范围。
#[derive(Debug, Clone, PartialEq)]
pub(super) struct NodeSpec {
    /// 目标节点。
    pub(super) node_id: NodeId,
    /// 数组/结构下标范围；`NumericRange::None` = 读/写整个值。
    pub(super) index_range: NumericRange,
}

/// 点表原始地址（连接前建立；`nsu=` 需连接后才能解析）。
#[derive(Debug, Clone)]
pub(super) struct RawPoint {
    /// values map 的 key（点位显示名）。
    pub(super) field: String,
    /// 原始 NodeId 串（含可能的 `nsu=` / 下标范围）。
    pub(super) raw: String,
    /// 点表声明类型（`ChildSpec.type`；写命令未带 `opc_type` 时的默认类型，OPC-7）。
    pub(super) declared_type: String,
}

/// 一台设备的原始点表。
#[derive(Debug, Clone)]
pub(super) struct RawDevice {
    /// 设备 ID（`rtdata:<devid>` 的 key）。
    pub(super) devid: String,
    /// 该设备的点位。
    pub(super) points: Vec<RawPoint>,
}

/// 写目标解析结果。
#[derive(Debug, Clone)]
pub(super) struct WriteTarget {
    /// 目标地址（含下标范围）。
    pub(super) spec: NodeSpec,
    /// 默认 OPC 类型（点表声明）；命令带 `opc_type` 时被覆盖。
    pub(super) opc_type: Option<String>,
}

/// 解析 NodeId 串（含可选的尾部下标范围）。
///
/// `namespaces` = 服务端 NamespaceArray（连接后 `read_namespace_array` 得到）；
/// 仅 `nsu=` 语法需要，纯 `ns=`/裸标识符传 `None` 即可。
pub(super) fn parse_node_spec(
    raw: &str,
    namespaces: Option<&NamespaceMap>,
) -> Result<NodeSpec, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(format!("invalid NodeId {raw:?}: 空串"));
    }
    let (addr, index_range) = split_index_range(trimmed);
    let addr = addr.trim();
    let Some(eq) = addr.find('=') else {
        return Err(format!(
            "invalid NodeId {raw:?}: 缺少 '='（形如 ns=2;s=Name / i=2258 / nsu=urn:foo;s=Name）"
        ));
    };
    let head_key = addr[..eq].trim().to_ascii_lowercase();
    let (ns, ident_seg) = match head_key.as_str() {
        "ns" | "nsu" => {
            let (head, rest) = split_ns_head(addr).ok_or_else(|| {
                format!("invalid NodeId {raw:?}: 命名空间头后缺少 ';i=|;s=|;g=|;b=' 标识符")
            })?;
            let Some((hk, hv)) = split_kv(head) else {
                return Err(format!("invalid NodeId {raw:?}: 命名空间头缺 '='"));
            };
            let ns = match hk.as_str() {
                "ns" => hv.parse::<u16>().map_err(|_| {
                    format!("invalid NodeId {raw:?}: 命名空间索引 {hv:?} 不是 0-65535 的整数")
                })?,
                // nsu=：URI → 服务端 NamespaceArray 中的索引
                _ => resolve_namespace_uri(hv, raw, namespaces)?,
            };
            (ns, rest)
        }
        // 裸标识符（i=/s=/g=/b=）默认命名空间 0
        "i" | "s" | "g" | "b" => (0u16, addr),
        other => {
            return Err(format!(
                "invalid NodeId {raw:?}: 未知前缀 {other:?}（支持 ns= / nsu= / i= / s= / g= / b=）"
            ));
        }
    };
    let identifier =
        parse_identifier(ident_seg).map_err(|e| format!("invalid NodeId {raw:?}: {e}"))?;
    Ok(NodeSpec {
        node_id: NodeId::new(ns, identifier),
        index_range,
    })
}

/// `nsu=` URI → 命名空间索引。
fn resolve_namespace_uri(
    uri: &str,
    raw: &str,
    namespaces: Option<&NamespaceMap>,
) -> Result<u16, String> {
    let map = namespaces.ok_or_else(|| {
        format!(
            "invalid NodeId {raw:?}: nsu= 需要服务端命名空间表（尚未连接或读取 NamespaceArray 失败）"
        )
    })?;
    map.get_index(uri).ok_or_else(|| {
        format!("invalid NodeId {raw:?}: 命名空间 URI {uri:?} 不在服务端 NamespaceArray 中")
    })
}

/// 拆出尾部的 `[n]` / `[a:b]` 下标范围；内容不是合法 NumericRange 时视为 NodeId 字面本身。
fn split_index_range(s: &str) -> (&str, NumericRange) {
    if s.ends_with(']') {
        if let Some(open) = s.rfind('[') {
            let inner = &s[open + 1..s.len() - 1];
            let inner = inner.trim();
            if !inner.is_empty() {
                if let Ok(range) = inner.parse::<NumericRange>() {
                    return (s[..open].trim_end(), range);
                }
            }
        }
    }
    (s, NumericRange::None)
}

/// 找命名空间头与标识符段的分隔 `;`（URI 里可能含 `;`，故要求右侧确实是标识符前缀）。
fn split_ns_head(s: &str) -> Option<(&str, &str)> {
    for (i, c) in s.char_indices() {
        if c == ';' {
            let rest = s[i + 1..].trim_start();
            if identifier_prefix(rest).is_some() {
                return Some((&s[..i], rest));
            }
        }
    }
    None
}

/// 拆 `key=value`（两侧去空白、key 小写）。
fn split_kv(s: &str) -> Option<(String, &str)> {
    let eq = s.find('=')?;
    Some((s[..eq].trim().to_ascii_lowercase(), s[eq + 1..].trim()))
}

/// 标识符前缀（i/s/g/b）与值；前缀大小写不敏感，`=` 两侧允许空白。
fn identifier_prefix(seg: &str) -> Option<(char, &str)> {
    let eq = seg.find('=')?;
    let key = seg[..eq].trim();
    if key.len() != 1 {
        return None;
    }
    let c = key.chars().next()?.to_ascii_lowercase();
    matches!(c, 'i' | 's' | 'g' | 'b').then(|| (c, seg[eq + 1..].trim()))
}

/// 解析标识符段（归一成库要求的 `i=…` / `s=…` 小写形式）。
fn parse_identifier(seg: &str) -> Result<Identifier, String> {
    let Some((k, v)) = identifier_prefix(seg) else {
        return Err(format!("标识符段 {seg:?} 不是 i=/s=/g=/b= 形态"));
    };
    format!("{k}={v}")
        .parse::<Identifier>()
        .map_err(|()| format!("标识符段 {seg:?} 无法解析（i= 需数字，g= 需 GUID，b= 需 base64）"))
}

/// 解析写命令的目标点位（F-1 契约）。
///
/// HMI / MQTT 只发 `{"tag":点位显示名,"value":…}`；`node_id` 是旧通道，仍优先支持。
/// 解析顺序：
/// 1. 命令带 `node_id` → 直接解析（点表里有同地址则回查声明类型）；
/// 2. 命令带 `tag` → 先在 `msg_devid` 对应设备里找，找不到再跨 devid 唯一兜底
///    （显示名全局唯一时允许 devid 不一致）；
/// 3. 都没有 / 解析不到 → `Err`（写入口据此回 422，不再静默丢弃）。
pub(super) fn resolve_write_target(
    devices: &[RawDevice],
    msg_devid: &str,
    node_id: Option<&str>,
    tag: Option<&str>,
    namespaces: Option<&NamespaceMap>,
) -> Result<WriteTarget, String> {
    if let Some(nid) = node_id.map(str::trim).filter(|s| !s.is_empty()) {
        let spec = parse_node_spec(nid, namespaces)?;
        let opc_type = declared_type_for_raw(devices, nid);
        return Ok(WriteTarget { spec, opc_type });
    }
    let Some(tag) = tag.map(str::trim).filter(|s| !s.is_empty()) else {
        return Err(
            "write 缺少 node_id/tag（HMI 契约：{\"tag\":点位显示名,\"value\":…}）".to_string(),
        );
    };
    // 目标设备内优先
    if let Some(dev) = devices.iter().find(|d| d.devid == msg_devid) {
        if let Some(p) = dev.points.iter().find(|p| p.field == tag) {
            return target_from_point(p, namespaces);
        }
    }
    // 跨 devid 唯一兜底
    let hits: Vec<(&str, &RawPoint)> = devices
        .iter()
        .flat_map(|d| {
            d.points
                .iter()
                .filter(|p| p.field == tag)
                .map(move |p| (d.devid.as_str(), p))
        })
        .collect();
    match hits.len() {
        1 => target_from_point(hits[0].1, namespaces),
        0 => Err(format!(
            "write tag {tag:?} 不在点表中（devid={msg_devid:?}）"
        )),
        _ => {
            let devids: Vec<&str> = hits.iter().map(|(d, _)| *d).collect();
            Err(format!(
                "write tag {tag:?} 在多个设备中重名（{}），请改用 node_id 指定",
                devids.join(", ")
            ))
        }
    }
}

fn target_from_point(
    p: &RawPoint,
    namespaces: Option<&NamespaceMap>,
) -> Result<WriteTarget, String> {
    let spec = parse_node_spec(&p.raw, namespaces)?;
    Ok(WriteTarget {
        spec,
        opc_type: (!p.declared_type.trim().is_empty()).then(|| p.declared_type.clone()),
    })
}

/// 按原始地址回查点表声明类型（多类型冲突时不猜）。
fn declared_type_for_raw(devices: &[RawDevice], raw: &str) -> Option<String> {
    let raw = raw.trim();
    let mut found: Option<&str> = None;
    for dev in devices {
        for p in &dev.points {
            if p.raw.trim() != raw || p.declared_type.trim().is_empty() {
                continue;
            }
            match found {
                None => found = Some(p.declared_type.as_str()),
                Some(t) if t != p.declared_type.as_str() => return None,
                _ => {}
            }
        }
    }
    found.map(str::to_string)
}
