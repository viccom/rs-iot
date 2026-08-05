use sha2::{Digest, Sha256};

/// 生成稳定的硬件 ID。
///
/// 对标 Lite_IOT `sysutil.GetHardwareID`（Go 用 CPU vendor + model）。
/// Rust 侧 sysinfo 只暴露 CPU brand（含 vendor 信息），故改用
/// `sha256(brand + ARCH)` 取前 8 位，前缀 `eiot`。
///
/// 语义：每机稳定唯一。与 Go 版不完全一致，但 rs-iot 为全新部署，无历史兼容问题。
#[must_use]
pub fn hardware_id() -> String {
    // sysinfo 0.32: 取首个 CPU 的 brand（含 vendor 信息）。new_all() 保证 cpu 列表已填充。
    // 仅在启动时调用一次，开销可接受。
    let sys = sysinfo::System::new_all();
    let brand = sys
        .cpus()
        .first()
        .map(|c| c.brand().trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| std::env::consts::ARCH.to_string());

    let mut hasher = Sha256::new();
    hasher.update(brand.as_bytes());
    hasher.update(std::env::consts::ARCH.as_bytes());
    let sum = hasher.finalize();

    let mut hex = String::with_capacity(64);
    for b in sum.iter() {
        hex.push_str(&format!("{:02x}", b));
    }
    format!("eiot{}", &hex[..8])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hwid_is_stable_and_prefixed() {
        let a = hardware_id();
        let b = hardware_id();
        assert!(a.starts_with("eiot"));
        assert_eq!(a.len(), 4 + 8);
        assert_eq!(a, b, "hwid must be stable across calls on the same host");
    }
}
