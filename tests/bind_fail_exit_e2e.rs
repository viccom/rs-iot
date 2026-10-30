//! N10 回归门（bind 失败进程必须退出）：
//!
//! **缺陷形态**（2026-10-29 warden 迁移期实锤，两次现场）：API 端口被占时
//! bind 仅 error! 打日志，进程继续运行（"rs-iot ready" 照打）——僵尸态：无
//! API 面但采集/归档全跑，与正常进程并存抢数据目录锁，且不响应运维预期
//! （以为起失败了却活着）。
//!
//! **门形态**：spawn 真实二进制两份（同 api_port、独立 lux 目录）——B 实例
//! bind 必然 Address already in use。断言 B 在 20s 内**自然退出且退出码非 0**
//! （优雅停机序列照走：history close/lux SAVE 不跳过——由日志语义保证）。
//! 红形态：B 不退出（超时 kill 后断言失败）。
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn unique_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "rsiot-bindfail-{tag}-{}-{nanos}",
        std::process::id()
    ))
}

fn write_config(dir: &std::path::Path, api_port: u16) -> PathBuf {
    let path = dir.join("config.toml");
    let lux = dir.join("lux");
    let logs = dir.join("logs");
    std::fs::create_dir_all(&lux).unwrap();
    std::fs::create_dir_all(&logs).unwrap();
    std::fs::write(
        &path,
        format!(
            "api_port = {api_port}\njwt_secret = \"bindfail-e2e-secret-32byte-pad!!\"\n\
             [lux]\nport = 0\ndata_dir = {:?}\n\n\
             [history]\nenabled = false\n\n\
             [mqtt]\nhost = \"\"\n\n\
             [log]\ndir = {:?}\nlevel = \"info\"\n",
            lux.to_string_lossy(),
            logs.to_string_lossy(),
        ),
    )
    .unwrap();
    path
}

fn spawn_rs_iot(config: &PathBuf) -> Child {
    Command::new(env!("CARGO_BIN_EXE_rs-iot"))
        .arg("--config")
        .arg(config)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn rs-iot binary")
}

fn wait_port_open(port: u16, deadline: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < deadline {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    false
}

#[test]
fn api_bind_failure_exits_nonzero_with_graceful_shutdown() {
    // 随机高位端口（探活后独占），避免并行测试冲突
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let dir_a = unique_dir("a");
    let dir_b = unique_dir("b");
    let cfg_a = write_config(&dir_a, port);
    let cfg_b = write_config(&dir_b, port);

    let mut first = spawn_rs_iot(&cfg_a);
    assert!(
        wait_port_open(port, Duration::from_secs(20)),
        "前置：A 实例占住端口 {port}"
    );

    let mut second = spawn_rs_iot(&cfg_b);
    let start = Instant::now();
    let mut exited: Option<std::process::ExitStatus> = None;
    while start.elapsed() < Duration::from_secs(20) {
        match second.try_wait().unwrap() {
            Some(status) => {
                exited = Some(status);
                break;
            }
            None => std::thread::sleep(Duration::from_millis(300)),
        }
    }

    let _ = first.kill();
    let _ = first.wait();
    let _ = second.kill();
    let _ = second.wait();
    std::fs::remove_dir_all(&dir_a).ok();
    std::fs::remove_dir_all(&dir_b).ok();

    let status = exited.expect("红：bind 失败的 B 实例 20s 内未退出（僵尸态复现）");
    assert!(
        !status.success(),
        "bind 失败必须非零退出（实际 {:?}）",
        status.code()
    );
}

/// N10 收尾门（2026-10-30）：bind 失败曾只取消子 token、未级联父 shutdown——
/// drain 窗口（等 pipeline 的 5s 超时）内补发 SIGTERM 会让信号任务把根 token
/// 也取消，`bind_fail.is_cancelled() && !shutdown.is_cancelled()` 判定翻转，
/// bind 失败以退出码 0 收场（监管器不告警）。
///
/// 门形态：B 实例 bind 失败（日志出现"触发优雅停机退出"即确认进入 drain）后
/// 立刻发 SIGTERM，断言退出码仍非 0。红形态：退出码 0。
#[cfg(unix)]
#[test]
fn api_bind_failure_keeps_nonzero_exit_when_sigterm_arrives_during_drain() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let dir_a = unique_dir("sigterm-a");
    let dir_b = unique_dir("sigterm-b");
    let cfg_a = write_config(&dir_a, port);
    let cfg_b = write_config(&dir_b, port);

    let mut first = spawn_rs_iot(&cfg_a);
    assert!(
        wait_port_open(port, Duration::from_secs(20)),
        "前置：A 实例占住端口 {port}"
    );

    let mut second = spawn_rs_iot(&cfg_b);

    // 轮询 B 的日志确认 bind 失败已发生（进入 drain 窗口），消除发信号时序竞态
    let logs_b = dir_b.join("logs");
    let start = Instant::now();
    let mut bind_fail_seen = false;
    while start.elapsed() < Duration::from_secs(20) {
        if let Ok(entries) = std::fs::read_dir(&logs_b) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with("rs-iot.log."))
                    .unwrap_or(false)
                {
                    if let Ok(content) = std::fs::read_to_string(&path) {
                        if content.contains("触发优雅停机退出") {
                            bind_fail_seen = true;
                            break;
                        }
                    }
                }
            }
        }
        if bind_fail_seen {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        bind_fail_seen,
        "前置：B 实例日志未见 bind 失败（{:?}）",
        logs_b
    );

    // drain 窗口内（5s pipeline 等待烧满期）补发 SIGTERM
    let pid = second.id();
    let _ = Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .expect("send SIGTERM");

    let mut exited: Option<std::process::ExitStatus> = None;
    while start.elapsed() < Duration::from_secs(20) {
        match second.try_wait().unwrap() {
            Some(status) => {
                exited = Some(status);
                break;
            }
            None => std::thread::sleep(Duration::from_millis(300)),
        }
    }

    let _ = first.kill();
    let _ = first.wait();
    let _ = second.kill();
    let _ = second.wait();
    std::fs::remove_dir_all(&dir_a).ok();
    std::fs::remove_dir_all(&dir_b).ok();

    let status = exited.expect("红：收到 SIGTERM 的 B 实例 20s 内未退出");
    assert!(
        !status.success(),
        "bind 失败 + drain 期 SIGTERM 必须仍非零退出（实际 {:?}）",
        status.code()
    );
}
