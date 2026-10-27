//! 陷阱 4 回归门（总方案 §四·陷阱 4 / 上游 tsink issue #17）：
//!
//! **本机复现形态**：真实二进制 debug 档带 `[history].enabled=true` 启动时
//! 主线程（Windows debug 主线程栈 1MiB）跑 `maybe_init` → tsink
//! StorageBuilder build 深链栈溢出，进程崩溃（thread 'main' has overflowed
//! its stack；enabled=false 正常起）。既有 e2e 全在测试 harness 线程（更大
//! 栈）上跑 init，故从未暴露。
//!
//! **门形态**：子进程模式（`stack_child_mode` + env `RSIOT_STACK_CHILD`）在
//! 显式 1MiB 栈线程（复刻 Windows debug 主线程栈预算）里调
//! `maybe_init(&cfg, &HashMap::new())`（enabled=true、唯一临时 data_dir、
//! 其余默认）——成功或 Err 都算"存活"（Some(Ok) 先显式 close 释放进程锁），
//! 随后 exit(0)；任何 panic/abort 由进程退出码体现。父测试 spawn
//! `current_exe()` 以 `--exact stack_child_mode -q` 运行子模式，try_wait
//! 轮询（30s 超时；栈溢出 abort 一般秒级，若触发 WER 卡死则超时 kill
//! 硬终止——退出码非 0 同样构成失败证据），断言退出码 == 0。
//!
//! **修复方向（绿阶段）**：init 深链移入 8MiB 专线线程（D3 单写线程同款
//! 缓解）——修复后本门转绿并永久固化，防 init 路径回归出深栈。
#![cfg(feature = "history")]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rs_iot::config::HistoryConfig;
use rs_iot::history::engine::maybe_init;

/// 子模式标记 env：值为 init 子进程的 tsink 数据目录。
const CHILD_ENV: &str = "RSIOT_STACK_CHILD";
/// 复刻 Windows debug 主线程栈预算的显式线程栈（陷阱 4 溢出形态）。
const INIT_STACK_BYTES: usize = 1024 * 1024;

/// 唯一临时目录：tag + pid + 纳秒时钟，测试并行与 CI 并发 run 不冲突；
/// best-effort 清理责任在父测（Windows 句柄释放有延迟，失败忽略）。
fn unique_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock after unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "rsiot-hist-stack-e2e-{tag}-{}-{nanos}",
        std::process::id()
    ))
}

/// 子模式 init 配置：enabled=true + 指定 data_dir + 其余默认（照 engine.rs
/// 既有测试的 cfg 构造惯例——enabled 必须显式 true，default 是 false）。
fn child_cfg(dir: &Path) -> HistoryConfig {
    HistoryConfig {
        enabled: true,
        data_dir: dir.to_string_lossy().into_owned(),
        ..HistoryConfig::default()
    }
}

/// 子模式测试（父测经 `--exact stack_child_mode` 单独驱动）：env
/// `RSIOT_STACK_CHILD` 有值 → 1MiB 线程 init 逻辑；无值（正常跑测）→
/// 直接返回。
///
/// 存活语义：`maybe_init` 返回 Some(Ok)（显式 close 释放进程锁/锁随目录
/// 消亡）、Some(Err)（init 失败交还调用方，fail-soft）都算存活，exit(0)；
/// 栈溢出 abort / panic 则进程退出码非 0，由父测断言暴露。
#[test]
fn stack_child_mode() {
    let dir = std::env::var(CHILD_ENV).unwrap_or_default();
    if dir.is_empty() {
        return;
    }
    let cfg = child_cfg(Path::new(&dir));

    // 1MiB 线程跑 init 深链：溢出发生在该线程时 Windows 上 abort 整个
    // 进程（join 永不返回），退出码即红证据——无需也无法在此捕获。
    let worker = std::thread::Builder::new()
        .stack_size(INIT_STACK_BYTES)
        .spawn(move || {
            let effective = HashMap::new();
            if let Some(Ok(mut engine)) = maybe_init(&cfg, &effective) {
                engine.close(Duration::from_secs(cfg.close_timeout_secs));
            }
            // Some(Err)：init 失败交还调用方（P8 fail-soft）——存活
            // None：enabled 判定旁路（cfg 已显式 true，实际不可达）——存活
        })
        .expect("spawn 1MiB init 线程必须成功");
    worker.join().expect("1MiB 线程 init 必须不 panic");

    std::process::exit(0);
}

/// 陷阱 4 回归门：1MiB 线程上 init 必须存活——子进程退出码必须 == 0；
/// 非 0（含 0xC00000FD/-1073741571 栈溢出 abort、101 panic、超时 kill）
/// 即 init 深链在窄栈上崩溃，断言失败并打出实际退出码。
#[test]
fn init_on_1mib_thread_survives() {
    let dir = unique_dir("init");

    // 1. spawn 子进程：同 binary 只跑 stack_child_mode（--exact），env 指向临时目录
    let mut child =
        std::process::Command::new(std::env::current_exe().expect("current_exe 必须可用"))
            .arg("--exact")
            .arg("stack_child_mode")
            .arg("-q")
            .env(CHILD_ENV, &dir)
            .spawn()
            .expect("spawn init 子进程必须成功");

    // 2. try_wait 轮询（30s 超时，500ms 间隔，照 kill e2e 等待惯例）：
    //    栈溢出 abort 一般秒级完成；若触发 WER 卡死 → 超时 kill 硬终止，
    //    退出码非 0 同样构成失败证据
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    child.kill().expect("超时后 kill 子进程必须成功");
                    break child.wait().expect("kill 后 wait 必须返回终态");
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            Err(e) => panic!("try_wait 轮询失败: {e}"),
        }
    };

    // 3. 门断言：退出码 == 0。Windows 栈溢出 abort 的 code() 通常呈
    //    0xC0000409（fastfail）或 0xC00000FD（STATUS_STACK_OVERFLOW）的
    //    i32 形态（-1073740791 / -1073741571）——任何非 0 都打进失败消息
    let code = status.code();
    assert!(
        code == Some(0),
        "陷阱 4 回归门：1MiB 线程 maybe_init 子进程必须以退出码 0 存活，\
         实际 code={code:?}（status: {status:?}；0xC0000409/-1073740791 或 \
         0xC00000FD/-1073741571 = 栈溢出 abort；101 = panic）"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
