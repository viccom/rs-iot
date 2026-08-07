//! mock IO backend（测试用，可编程响应）。
//!
//! `#[allow(dead_code)]`：lib build 无 mock 调用点（luaClient run 用真实 backend）；
//! io_engine/api tests 用——dual-build（lib dead + test alive），expect 在 test build
//! unfulfilled，故用 allow。

use super::IoBackend;

/// mock 响应闭包类型（frame 字节 → 响应字节 / Err）。拆 alias 规避 clippy type_complexity。
#[allow(dead_code)]
pub(in crate::plugin::builtin::lua_client) type MockResponder =
    Box<dyn Fn(&[u8]) -> mlua::Result<Vec<u8>> + Send + Sync>;

/// mock IO backend：按闭包响应。用于无真实设备的单测/集成测。
#[allow(dead_code)]
pub(in crate::plugin::builtin::lua_client) struct MockIoTask {
    /// 响应闭包：输入 frame 字节，返回响应字节（或 Err）。
    pub(in crate::plugin::builtin::lua_client) responder: MockResponder,
}

#[async_trait::async_trait]
impl IoBackend for MockIoTask {
    async fn request(&mut self, frame: &[u8]) -> mlua::Result<Vec<u8>> {
        (self.responder)(frame)
    }
}
