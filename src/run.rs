use anyhow::Result;

/// `run` 子命令：委托给 lib::run_app。
pub(crate) async fn run(cfg: rs_iot::config::AppConfig) -> Result<()> {
    rs_iot::run_app(cfg).await?;
    Ok(())
}
