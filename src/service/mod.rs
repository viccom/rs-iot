//! 服务化：Windows Service / Linux systemd。
//!
//! - `install`/`uninstall`：注册/卸载系统服务
//! - `windows::dispatch`：被 SCM 调用的服务入口（Windows only）

#[cfg(unix)]
pub(crate) mod systemd;
#[cfg(windows)]
pub(crate) mod windows;

/// 安装为系统服务。
pub(crate) fn install() -> anyhow::Result<()> {
    let exe = std::env::current_exe()?;
    println!("binary: {}", exe.display());
    #[cfg(unix)]
    {
        systemd::install(&exe)?;
    }
    #[cfg(windows)]
    {
        install_windows(&exe)?;
    }
    Ok(())
}

/// 卸载系统服务。
pub(crate) fn uninstall() -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        systemd::uninstall()?;
    }
    #[cfg(windows)]
    {
        uninstall_windows()?;
    }
    Ok(())
}

#[cfg(windows)]
fn install_windows(exe: &std::path::Path) -> anyhow::Result<()> {
    // binPath 形如 "C:\path\rs-iot.exe service"（SCM 以 `service` 子命令调用 dispatch）
    let binpath = format!("\"{}\" service", exe.display());
    let status = std::process::Command::new("sc")
        .args([
            "create",
            "rs-iot",
            "binPath=",
            &binpath,
            "start=",
            "auto",
            "DisplayName=",
            "rs-iot",
        ])
        .status()?;
    if !status.success() {
        anyhow::bail!("sc create 失败（需管理员权限运行）");
    }
    println!("已安装服务 rs-iot。启动：sc start rs-iot；停止：sc stop rs-iot");
    Ok(())
}

#[cfg(windows)]
fn uninstall_windows() -> anyhow::Result<()> {
    let _ = std::process::Command::new("sc")
        .args(["stop", "rs-iot"])
        .status();
    let status = std::process::Command::new("sc")
        .args(["delete", "rs-iot"])
        .status()?;
    if !status.success() {
        anyhow::bail!("sc delete 失败");
    }
    println!("已卸载服务 rs-iot");
    Ok(())
}
