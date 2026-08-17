//! Linux systemd 服务安装（生成 unit 文件）。

use std::path::Path;

const UNIT_PATH: &str = "/etc/systemd/system/rs-iot.service";

pub fn install(exe: &Path) -> anyhow::Result<()> {
    let unit = format!(
        "[Unit]\n\
         Description=rs-iot industrial IoT edge framework\n\
         After=network.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={exe} run\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n",
        exe = exe.display()
    );
    std::fs::write(UNIT_PATH, unit)
        .map_err(|e| anyhow::anyhow!("写 {UNIT_PATH} 失败: {e}（需 root）"))?;
    println!("已写入 {UNIT_PATH}");
    println!("启用并启动: sudo systemctl enable --now rs-iot");
    Ok(())
}

pub fn uninstall() -> anyhow::Result<()> {
    let _ = std::fs::remove_file(UNIT_PATH);
    println!("已删除 {UNIT_PATH}");
    println!("请执行: sudo systemctl disable rs-iot");
    Ok(())
}
