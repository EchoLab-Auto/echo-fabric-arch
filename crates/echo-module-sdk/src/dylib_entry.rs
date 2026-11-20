//! cdylib 入口：模块编译成动态库，由宿主 `dlopen` 后**在同一进程内**运行。
//!
//! 与 [`crate::serve_stdio`] 的关系：**协议完全一致**（同一套帧、同一套握手、
//! 同一套调用语义），只是载体从 stdin/stdout 换成宿主建好的一条 socketpair。
//! 所以同一份 [`ModuleHandler`](crate::ModuleHandler) 实现可以三选一：
//!
//! ```text
//! bin/main.rs      → serve_stdio(...)         独立进程（有崩溃隔离）
//! 编进宿主         → ModuleHandler 直接构造    同进程（无独立产物）
//! cdylib/*.rs      → serve_fd_blocking(...)   独立产物 + 同进程
//! ```
//!
//! cdylib 侧的最小实现：
//!
//! ```no_run
//! # use std::ffi::c_int;
//! # use echo_module_sdk::{async_trait, serve_fd_blocking};
//! # struct MyModule;
//! # #[async_trait]
//! # impl echo_module_sdk::ModuleHandler for MyModule {
//! #     fn contributions(&self) -> Vec<echo_module_sdk::Contribution> { vec![] }
//! #     async fn invoke(&self, _r: echo_module_sdk::InvokeRequest)
//! #         -> echo_module_sdk::InvokeOutcome { echo_module_sdk::InvokeOutcome::ok("ok") }
//! # }
//! #[no_mangle]
//! pub extern "C" fn echo_module_run(fd: c_int) -> c_int {
//!     serve_fd_blocking(|| MyModule, fd)
//! }
//! ```
//!
//! ## 为什么必须有 `catch_unwind`
//!
//! **panic 跨 FFI 边界是未定义行为**。宿主是用 `dlopen` + 函数指针调用进来的，
//! 一次 `unwrap()` 失败若穿出 `extern "C"` 就是 UB——可能是段错误，也可能是
//! 悄悄破坏宿主的栈。所以本函数把整个协议循环包在 `catch_unwind` 里，把 panic
//! 转成非 0 退出码 + 一行 stderr。
//!
//! 注意这**不等于**崩溃隔离：`catch_unwind` 只接得住 unwind 型 panic，
//! segfault / abort / OOM 照样带走宿主。要隔离就得用 `serve_stdio`。

#![cfg(unix)]

use std::ffi::c_int;
use std::os::unix::io::{FromRawFd, RawFd};

use crate::serve::serve_io;
use crate::ModuleHandler;

/// 正常退出码。
pub const EXIT_OK: c_int = 0;
/// 协议 / IO 出错。
pub const EXIT_ERROR: c_int = 1;
/// 模块内部 panic。
pub const EXIT_PANIC: c_int = 2;

/// 从宿主交入的 socket fd 接管一条模块连接，跑完整协议循环后返回退出码。
///
/// `make` 在 runtime 建好之后才调用——模块可以在构造函数里就用到 tokio 设施
/// （`tokio::spawn` / 定时器），不会因为"构造时还没有 runtime"而 panic。
///
/// 阻塞：本函数内部 `block_on`，调用方（宿主）已经把它放在专用线程上，所以
/// 这里不再额外起线程。
///
/// fd 所有权：**接管**。返回时（含 panic 路径）由内部持有者关闭。
pub fn serve_fd_blocking<H, F>(make: F, fd: RawFd) -> c_int
where
    F: FnOnce() -> H,
    H: ModuleHandler,
{
    // 整个函数体都在 `catch_unwind` 里，包括建 runtime 与接管道这一段。
    //
    // 这不是"防御性编程"：`extern "C"` 在 Rust 里是 **nounwind**，任何穿出去的
    // panic 都会被编译器改成 `abort` ——带走的不是模块，是**整个宿主进程**。
    // 2026-11 实测踩到：`UnixStream::from_std` 在 reactor 之外调用会 panic，
    // 那一版把它放在了 catch_unwind 外面，于是宿主直接 SIGABRT。
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(make, fd))) {
        Ok(code) => code,
        Err(panic) => {
            let reason = panic
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "<non-string panic payload>".to_string());
            eprintln!("module: panicked before the protocol loop settled: {reason}");
            EXIT_PANIC
        }
    }
}

/// [`serve_fd_blocking`] 的实际实现（已在 `catch_unwind` 之内）。
fn run<H, F>(make: F, fd: RawFd) -> c_int
where
    F: FnOnce() -> H,
    H: ModuleHandler,
{
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("module-runtime")
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("module: cannot build a runtime: {error}");
            return EXIT_ERROR;
        }
    };
    // `from_std` 会把 fd 注册进 reactor，**必须在 runtime 上下文里调用**。
    // 用 `enter()` 而不是把它塞进 `block_on`：这样 reader/writer 的构造在
    // block_on 之外完成，`make()` 仍然在 runtime 上下文里执行（模块构造函数
    // 里 `tokio::spawn` 是合法的）。
    let _reactor = runtime.enter();

    // SAFETY: fd 由宿主创建并移交（契约见 `echo-module-host::dylib` 的
    // `ENTRY_SYMBOL`）；本函数是它唯一的所有者，关闭也只在这里发生。
    let std_stream = unsafe { std::os::unix::net::UnixStream::from_raw_fd(fd) };
    if let Err(error) = std_stream.set_nonblocking(true) {
        eprintln!("module: cannot set the socket non-blocking: {error}");
        return EXIT_ERROR;
    }
    let stream = match tokio::net::UnixStream::from_std(std_stream) {
        Ok(stream) => stream,
        Err(error) => {
            eprintln!("module: cannot register the socket with the reactor: {error}");
            return EXIT_ERROR;
        }
    };
    let (reader, writer) = stream.into_split();

    let outcome = runtime.block_on(async move { serve_io(make(), reader, writer).await });

    let code = match outcome {
        Ok(()) => EXIT_OK,
        Err(error) => {
            eprintln!("module: protocol loop ended with an error: {error}");
            EXIT_ERROR
        }
    };

    // 在途任务可能还持有 socket 的写半；给它们一点点收尾时间，别在这里无限等
    // （模块已经决定退出了，赖着不走只会拖慢宿主的关停）。
    runtime.shutdown_timeout(std::time::Duration::from_millis(200));
    code
}
