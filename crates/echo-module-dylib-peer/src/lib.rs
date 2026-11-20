//! 测试夹具：一个**真的 cdylib** 模块（`.so`），供 `dylib` 运输层验收使用。
//!
//! 它不是可发布的模块（`publish = false`），唯一职责是证明**同一份
//! `ModuleHandler` 实现**能被打包成动态库、由宿主 `dlopen` 后在同一进程里跑
//! 完整协议。行为由 `Hello.config` 控制：
//!
//! | config | 行为 |
//! |---|---|
//! | `{}` / 缺省 | 握手 + 回显（返回 `echo:<payload>`） |
//! | `{"hang": true}` | 收到 Invoke 后不回复（验证宿主侧超时 → Cancel） |
//! | `{"panic_on_invoke": true}` | 收到 Invoke 后 panic（验证宿主存活） |
//!
//! 导出符号：`echo_module_run(fd) -> c_int`（见 `echo_module_host::dylib::ENTRY_SYMBOL`）。

use std::sync::Mutex;

use echo_module_sdk::echo_module_api::ServiceContribution;
use echo_module_sdk::{
    async_trait, serve_fd_blocking, Contribution, InvokeOutcome, InvokeRequest, ModuleHandler,
};
use serde_json::Value;

/// 从 `Hello.config` 解析出的行为开关。
#[derive(Default)]
struct Config {
    hang: bool,
    panic_on_invoke: bool,
}

/// 夹具模块。`config` 在 `configure` 阶段才到，所以用 `Mutex` 后填。
struct Peer {
    config: Mutex<Config>,
}

#[async_trait]
impl ModuleHandler for Peer {
    async fn configure(&self, config: Value) -> Result<(), String> {
        let mut slot = self
            .config
            .lock()
            .map_err(|_| "dylib peer: config lock poisoned".to_string())?;
        slot.hang = config.get("hang").and_then(Value::as_bool).unwrap_or(false);
        slot.panic_on_invoke = config
            .get("panic_on_invoke")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        Ok(())
    }

    fn contributions(&self) -> Vec<Contribution> {
        // SDK 只把 Invoke 转发给**已声明**的贡献名（未声明 → `unknown_contribution`），
        // 所以这里必须真的声明一个——用一个框架中立的 `Service` 贡献，不引入
        // 任何领域词汇。
        vec![Contribution::Service(ServiceContribution {
            key: "echo".to_string(),
            kind: "test".to_string(),
            version: 1,
        })]
    }

    async fn invoke(&self, req: InvokeRequest) -> InvokeOutcome {
        let (hang, panic_on_invoke) = self
            .config
            .lock()
            .map(|slot| (slot.hang, slot.panic_on_invoke))
            .unwrap_or((false, false));
        if panic_on_invoke {
            // 故意 panic：宿主必须在 `.so` 内的 catch_unwind 之后仍然存活。
            panic!("dylib peer: panic_on_invoke");
        }
        if hang {
            // 永不返回：验证宿主侧的调用超时与 Cancel 广播。
            std::future::pending::<()>().await;
        }
        if req
            .payload
            .get("whoami")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            // 报告"我在哪个进程的哪个线程里跑"——宿主用它证明两件事：
            // 模块确实同进程（pid 相同），以及关停后那个线程真的没了（tid）。
            let tid = unsafe { libc::gettid() };
            return InvokeOutcome::ok(format!("pid={} tid={tid}", std::process::id()));
        }
        InvokeOutcome::ok(format!("echo:{}", req.payload))
    }
}

/// 宿主 `dlopen` 后调用的唯一入口。
///
/// 宿主侧按 `unsafe extern "C" fn(c_int) -> c_int` 取符号；这里声明为安全函数
/// 是对的——"传入的 fd 有效且唯一"这条前置条件由宿主保证，模块侧无从检查。
///
/// # Safety（供宿主参考）
///
/// `fd` 必须是一条**已连接**的流式 socket，且调用方在调用后不再使用它（所有权
/// 转移；内部持有者负责关闭）。
#[no_mangle]
pub extern "C" fn echo_module_run(fd: std::ffi::c_int) -> std::ffi::c_int {
    serve_fd_blocking(
        || Peer {
            config: Mutex::new(Config::default()),
        },
        fd,
    )
}

#[cfg(test)]
mod tests {
    /// 编译期守卫：入口符号存在且签名是 `extern "C" fn(c_int) -> c_int`。
    ///
    /// 这是宿主与模块之间唯一的跨边界契约——改了它，宿主 `dlopen` 就找不到。
    #[test]
    fn entry_signature_is_the_frozen_contract() {
        let entry: extern "C" fn(std::ffi::c_int) -> std::ffi::c_int = crate::echo_module_run;
        let _ = entry;
    }
}
