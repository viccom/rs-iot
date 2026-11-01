//! 写目标索引（F-1）：HMI 只发 `{"tag": 点位显示名, "value": …}`，CIP 路径由插件解析。
//!
//! 台账（§2 F-1）证据：前端 `command.ts:54` 只发 `{tag,value}`，而 eipClient 此前把
//! `tag` 直接当 CIP 路径写（`mod.rs:680`）——显示名 `uptime_s` 会被写成同名 tag，
//! PLC 回 0x05/0x04 而 REST 已返回 200。
//!
//! 解析顺序：
//! 1. 本设备的点位显示名（`label`，空则 `value`）→ 该点位的 CIP 路径；
//! 2. 全表唯一同名显示名（模型模板 × params 展开下同一物理 tag 挂多设备是常态）；
//! 3. 命令里的串本身就是**已配置**的 CIP 路径（旧客户端兼容）；
//! 4. 都不行 → `Err`（不得盲目拿显示名当路径写）。

use crate::types::DeviceSpec;
use std::collections::HashMap;

/// 写目标索引。
#[derive(Debug, Default)]
pub(super) struct WriteIndex {
    /// devid → (点位显示名 → CIP 路径)。
    by_devid: HashMap<String, HashMap<String, String>>,
    /// CIP 路径 → 出现该路径的 devid 列表（"原样路径"合法性判定）。
    paths: HashMap<String, Vec<String>>,
}

impl WriteIndex {
    /// 从（展开后的）设备点表建索引：`values` 的 key 即点位显示名。
    pub(super) fn from_devices(devices: &[DeviceSpec]) -> Self {
        let mut idx = Self::default();
        for dev in devices {
            for c in &dev.children {
                if c.value.is_empty() {
                    continue;
                }
                let field = if c.label.is_empty() {
                    c.value.clone()
                } else {
                    c.label.clone()
                };
                idx.by_devid
                    .entry(dev.value.clone())
                    .or_default()
                    .insert(field, c.value.clone());
                idx.paths
                    .entry(c.value.clone())
                    .or_default()
                    .push(dev.value.clone());
            }
        }
        idx
    }

    /// 解析写目标：显示名 → CIP 路径（失败给出可执行的提示）。
    pub(super) fn resolve(&self, devid: &str, tag: &str) -> Result<String, String> {
        let tag = tag.trim();
        if tag.is_empty() {
            return Err("写命令 tag 为空".to_string());
        }
        // 1) 本设备点位显示名
        if let Some(path) = self.by_devid.get(devid).and_then(|m| m.get(tag)) {
            return Ok(path.clone());
        }
        // 2) 全表唯一显示名兜底（跨设备同一点位）
        let mut hits: Vec<&String> = self.by_devid.values().filter_map(|m| m.get(tag)).collect();
        hits.dedup();
        match hits.as_slice() {
            [path] => return Ok((*path).clone()),
            [] => {}
            many => {
                return Err(format!(
                    "写命令 tag '{tag}' 在设备 {devid} 无此点位，且全表有 {} 处同名显示名，无法确定目标（F-1）",
                    many.len()
                ));
            }
        }
        // 3) 旧客户端：tag 直接是已配置的 CIP 路径
        if self.paths.contains_key(tag) {
            return Ok(tag.to_string());
        }
        Err(format!(
            "写命令 tag '{tag}' 既不是设备 {devid} 的点位显示名，也不是已配置的 CIP 路径（F-1）；\
             请传点位显示名（label）或点表里的 tag 全名"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ChildSpec;

    fn child(label: &str, value: &str) -> ChildSpec {
        ChildSpec {
            label: label.into(),
            value: value.into(),
            r#type: String::new(),
            props: None,
        }
    }

    fn devices() -> Vec<DeviceSpec> {
        vec![
            DeviceSpec {
                label: "plcA".into(),
                value: "plcA".into(),
                children: vec![
                    child("uptime_s", "Program:Main.Uptime"),
                    child("setpoint", "SP_TAG"),
                    // 显示名与 CIP 路径同名（旧配置常见）
                    child("", "PlainTag"),
                ],
                slave_id: None,
            },
            DeviceSpec {
                label: "plcB".into(),
                value: "plcB".into(),
                children: vec![child("shared", "SharedTag")],
                slave_id: None,
            },
        ]
    }

    /// F-1：显示名 → CIP 路径；CIP 路径原样；两者都不行 → Err。
    #[test]
    fn resolve_display_name_and_raw_path() {
        let idx = WriteIndex::from_devices(&devices());
        assert_eq!(
            idx.resolve("plcA", "uptime_s").unwrap(),
            "Program:Main.Uptime",
            "显示名必须映射到 CIP 路径"
        );
        assert_eq!(idx.resolve("plcA", "setpoint").unwrap(), "SP_TAG");
        // 旧客户端直接发 CIP 路径
        assert_eq!(idx.resolve("plcA", "SP_TAG").unwrap(), "SP_TAG");
        // label 为空时 value 即显示名
        assert_eq!(idx.resolve("plcA", "PlainTag").unwrap(), "PlainTag");
        // 未知串不得当路径写
        let err = idx
            .resolve("plcA", "NoSuchPoint")
            .expect_err("未知显示名必须 Err");
        assert!(err.contains("F-1"), "{err}");
    }

    /// F-1：跨设备同名显示名——唯一时兜底，歧义时拒绝（不得猜）。
    #[test]
    fn resolve_cross_device_fallback() {
        let idx = WriteIndex::from_devices(&devices());
        // plcA 没有 shared，但全表唯一 → 兜底命中 plcB 的路径
        assert_eq!(idx.resolve("plcA", "shared").unwrap(), "SharedTag");

        let mut devs = devices();
        devs[0].children.push(child("shared", "OtherShared"));
        let dup = WriteIndex::from_devices(&devs);
        let err = dup
            .resolve("plcC", "shared")
            .expect_err("歧义显示名必须拒绝");
        assert!(err.contains("无法确定目标"), "{err}");
    }
}
