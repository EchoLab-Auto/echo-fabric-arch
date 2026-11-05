//! stdio 运输层：模块作为独立子进程运行，经 stdin/stdout 交换协议帧。
//!
//! ## 帧编码（本次定稿）
//!
//! 帧 = 4 字节**小端** `u32` 长度前缀 + JSON 帧体；前缀计的是帧体字节数，
//! JSON 形状与 [`HostToModule`] / [`ModuleToHost`] 的 serde 表示一致。
//! 单帧上限 [`MAX_FRAME_BYTES`]（16 MiB）：读侧长度前缀超限、写侧序列化结果
//! 超限均返回 [`TransportError::Protocol`]。
//!
//! 读写经 `read_exact` / `write_all` 实现（容忍「长度先到、帧体后到」的分段到达）；
//! stdin 写入端与 stdout 读取端各由一把 [`tokio::sync::Mutex`] 串行化，
//! 并发 `send` / `recv` 不会交错损坏帧。
//!
//! ## 进程管理
//!
//! - spawn：stdin/stdout/stderr 全部管道化 + `kill_on_drop(true)`；Unix 下
//!   `process_group(0)`（模块独立成组，便于整组清理）。
//! - stderr：后台任务逐行读取，转发 `tracing::warn!`（带 `module = <id>` 字段）。
//! - 环境：默认**不继承**宿主环境（`env_clear`），仅透传 `PATH`；其余变量须经
//!   [`StdioTransport::with_env`] 显式传入，避免宿主环境泄漏给模块。
//! - 退出观察：对端退出后 `recv` / `send` 返回 [`TransportError::Closed`]。
//! - [`StdioConnection::shutdown`]：关闭 stdin → 等子进程退出（带 deadline）→
//!   超时 `start_kill()` + `wait()`（类比 SIGKILL 语义）并返回
//!   [`TransportError::Timeout`]；重复调用安全（幂等，第二次直接 `Ok(())`）。

use std::ffi::OsString;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

use crate::api::{HostToModule, ModuleToHost};
use crate::transport::{ModuleConnection, ModuleSpec, Transport, TransportError};

/// 单帧上限（16 MiB）。读侧长度前缀与写侧序列化结果均受此约束。
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// 子进程资源限额（运行时形态；配置侧等价于 `modules.toml` 的
/// `[module.limits]`）。
///
/// 全体 `Option`：**`None` = 不限制**（继承宿主）。只支持五项，每项都是
/// 硬限额（软=硬）——模块没有义务处理 SIGXCPU，两段式限额只会让人误以为
/// 没生效。
///
/// 限额在 `exec` **之前**由子进程自己设置（`setrlimit` 继承给它的后代），
/// 因此模块再派生的子进程同样受限。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ResourceLimits {
    /// CPU 时间（秒）。
    pub cpu_seconds: Option<u64>,
    /// 地址空间（字节）。
    pub memory_bytes: Option<u64>,
    /// 单文件写入上限（字节）。
    pub file_size_bytes: Option<u64>,
    /// 打开文件描述符数（含 socket）。
    pub open_files: Option<u64>,
    /// 子进程数。
    pub processes: Option<u64>,
}

impl ResourceLimits {
    /// 是否一项都没设（此时不装 `pre_exec`，spawn 路径与无限制完全一致）。
    pub fn is_empty(&self) -> bool {
        self.cpu_seconds.is_none()
            && self.memory_bytes.is_none()
            && self.file_size_bytes.is_none()
            && self.open_files.is_none()
            && self.processes.is_none()
    }
}

/// stdio 运输层：每次 [`Transport::start`] spawn 一个模块子进程。
///
/// 命令始终按可执行文件路径解析（[`PathBuf`]，不经 shell、无通配 / 变量展开）：
///
/// ```no_run
/// # use echo_module_host::stdio::StdioTransport;
/// let transport = StdioTransport::new("/usr/bin/my-module")
///     .with_args(["serve", "--stdio"])
///     .with_env("MY_MODULE_MODE", "test")
///     .with_cwd("/tmp");
/// ```
#[derive(Debug, Clone)]
pub struct StdioTransport {
    program: PathBuf,
    args: Vec<OsString>,
    env: Vec<(OsString, OsString)>,
    cwd: Option<PathBuf>,
    limits: ResourceLimits,
}

impl StdioTransport {
    /// 以可执行文件路径构造（相对路径按宿主当前工作目录解析）。
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            env: Vec::new(),
            cwd: None,
            limits: ResourceLimits::default(),
        }
    }

    /// 追加命令行参数（链式；多次调用累积）。
    pub fn with_args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// 追加一个子进程环境变量（链式；宿主环境默认不继承，仅透传 `PATH`）。
    pub fn with_env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// 设置子进程工作目录（链式）。
    pub fn with_cwd(mut self, dir: impl Into<PathBuf>) -> Self {
        self.cwd = Some(dir.into());
        self
    }

    /// 设置子进程资源限额（链式；缺省 = 不限制）。
    ///
    /// 空限额（一项都没设）等价于没调用：不装 `pre_exec`，spawn 路径保持
    /// 无限制时的原样。
    pub fn with_limits(mut self, limits: ResourceLimits) -> Self {
        self.limits = limits;
        self
    }

    /// 组装 spawn 命令（管道 stdio / 最小环境 / 进程组 / 工作目录）。
    fn command(&self) -> Command {
        let mut cmd = Command::new(&self.program);
        cmd.args(&self.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // 最小环境：不继承宿主环境，仅透传 PATH（可执行查找 / 模块自身依赖）；
        // 其余变量经 `with_env` 显式声明，不额外泄漏。
        cmd.env_clear();
        if let Some(path) = std::env::var_os("PATH") {
            cmd.env("PATH", path);
        }
        for (key, value) in &self.env {
            cmd.env(key, value);
        }
        if let Some(cwd) = &self.cwd {
            cmd.current_dir(cwd);
        }
        #[cfg(unix)]
        cmd.process_group(0); // 独立进程组：shutdown 超时强杀 / 整组清理的前提。
        #[cfg(unix)]
        if !self.limits.is_empty() {
            let limits = self.limits;
            // SAFETY: `pre_exec` 的闭包在 fork 之后、exec 之前运行，只允许
            // 异步信号安全的操作。这里只调 `setrlimit`（POSIX 允许在
            // fork 与 exec 之间调用），不分配内存、不取锁、不碰 Rust 运行时。
            unsafe {
                cmd.pre_exec(move || apply_limits(&limits));
            }
        }
        cmd
    }
}

/// 在子进程里落地限额（`fork` 后、`exec` 前；只做 `setrlimit`）。
///
/// 返回值是 `io::Error`：`pre_exec` 的契约要求如此，任一限额设置失败即让
/// spawn 失败——**静默忽略等于假装限额生效**，那比不设更危险。
#[cfg(unix)]
fn apply_limits(limits: &ResourceLimits) -> std::io::Result<()> {
    // `setrlimit` 在 linux 绑定里第一个形参是 `__rlimit_resource_t`（= u32），
    // 而 `RLIMIT_*` 常量也是 u32；直接沿用绑定类型，避免跨平台猜宽度。
    fn set(resource: libc::__rlimit_resource_t, value: Option<u64>) -> std::io::Result<()> {
        let Some(value) = value else {
            return Ok(());
        };
        // rlim_t 在不同平台上宽度不同：u64 → libc::rlim_t 显式转换，
        // 溢出即按"上限"处理（不做静默截断）。
        let limit = libc::rlim_t::try_from(value).unwrap_or(libc::rlim_t::MAX);
        let rlimit = libc::rlimit {
            rlim_cur: limit,
            rlim_max: limit,
        };
        // SAFETY: 传的是本函数栈上的有效 rlimit 指针；setrlimit 不保留它。
        let rc = unsafe { libc::setrlimit(resource, &rlimit) };
        if rc == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
    set(libc::RLIMIT_CPU, limits.cpu_seconds)?;
    set(libc::RLIMIT_AS, limits.memory_bytes)?;
    set(libc::RLIMIT_FSIZE, limits.file_size_bytes)?;
    set(libc::RLIMIT_NOFILE, limits.open_files)?;
    // RLIMIT_NPROC 限的是**该 uid 的**总进程数，不是本进程树的——多模块
    // 同 uid 部署时它会把彼此算进配额（可能误伤）。仍然提供，但值要给得
    // 足够宽（文档已注明）。
    set(libc::RLIMIT_NPROC, limits.processes)?;
    Ok(())
}

#[async_trait]
impl Transport for StdioTransport {
    async fn start(&self, spec: ModuleSpec) -> Result<Box<dyn ModuleConnection>, TransportError> {
        let mut cmd = self.command();
        let mut child = cmd.spawn().map_err(|e| {
            TransportError::Io(format!(
                "failed to spawn module process {:?}: {e}",
                self.program
            ))
        })?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| TransportError::Io("module stdin pipe unavailable".to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| TransportError::Io("module stdout pipe unavailable".to_string()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| TransportError::Io("module stderr pipe unavailable".to_string()))?;

        spawn_stderr_forwarder(&spec.module_id, stderr);

        Ok(Box::new(StdioConnection {
            stdin: Mutex::new(Some(stdin)),
            stdout: Mutex::new(stdout),
            child: Mutex::new(Some(child)),
        }))
    }
}

/// 宿主侧 stdio 连接：stdin 写入端与 stdout 读取端各一把锁，避免并发交错损坏帧。
pub struct StdioConnection {
    /// 宿主 → 模块（stdin）；`None` = shutdown 已关闭该方向。
    stdin: Mutex<Option<ChildStdin>>,
    /// 模块 → 宿主（stdout）。
    stdout: Mutex<ChildStdout>,
    /// 子进程句柄；`None` = 已收割（shutdown 完成或已强杀）。
    child: Mutex<Option<Child>>,
}

impl StdioConnection {
    /// 读一帧：长度前缀（4 字节 LE）→ 帧体 → JSON 解码。
    ///
    /// 分段到达由 `read_exact` 等待补齐；EOF（对端退出）= [`TransportError::Closed`]。
    async fn read_frame(&self) -> Result<ModuleToHost, TransportError> {
        let mut stdout = self.stdout.lock().await;

        let mut len_bytes = [0u8; 4];
        stdout
            .read_exact(&mut len_bytes)
            .await
            .map_err(|e| map_read_error("read frame length", e))?;
        let len = u32::from_le_bytes(len_bytes) as usize;
        if len > MAX_FRAME_BYTES {
            return Err(TransportError::Protocol(format!(
                "frame length {len} exceeds limit {MAX_FRAME_BYTES}"
            )));
        }

        let mut body = vec![0u8; len];
        stdout
            .read_exact(&mut body)
            .await
            .map_err(|e| map_read_error("read frame body", e))?;

        serde_json::from_slice(&body)
            .map_err(|e| TransportError::Protocol(format!("invalid JSON frame: {e}")))
    }

    /// 写一帧：JSON 编码 → 4 字节 LE 长度前缀 + 帧体（同一把锁内一次性写出）。
    async fn write_frame(&self, msg: &HostToModule) -> Result<(), TransportError> {
        let body = serde_json::to_vec(msg)
            .map_err(|e| TransportError::Protocol(format!("encode message: {e}")))?;
        if body.len() > MAX_FRAME_BYTES {
            return Err(TransportError::Protocol(format!(
                "frame length {} exceeds limit {MAX_FRAME_BYTES}",
                body.len()
            )));
        }

        let mut stdin = self.stdin.lock().await;
        let Some(pipe) = stdin.as_mut() else {
            return Err(TransportError::Closed);
        };
        let mut frame = Vec::with_capacity(4 + body.len());
        frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
        frame.extend_from_slice(&body);
        pipe.write_all(&frame).await.map_err(map_write_error)?;
        pipe.flush().await.map_err(map_write_error)
    }
}

#[async_trait]
impl ModuleConnection for StdioConnection {
    async fn send(&self, msg: HostToModule) -> Result<(), TransportError> {
        self.write_frame(&msg).await
    }

    async fn recv(&self) -> Result<ModuleToHost, TransportError> {
        self.read_frame().await
    }

    /// 关停语义（类比 SIGTERM → SIGKILL）：
    /// 1. 丢弃 stdin（模块收到 EOF，可自行退出）；
    /// 2. 等待子进程退出，最多 `deadline`；
    /// 3. 超时 → `start_kill()` 强杀 + `wait()` 收尸，返回 [`TransportError::Timeout`]。
    ///
    /// 重复调用：子进程已收割时直接 `Ok(())`（幂等）。
    async fn shutdown(&self, deadline: Duration) -> Result<(), TransportError> {
        // 关闭宿主 → 模块方向（stdin EOF）。
        self.stdin.lock().await.take();

        let mut guard = self.child.lock().await;
        let Some(mut child) = guard.take() else {
            return Ok(()); // 已关停 / 已退出：幂等
        };
        match tokio::time::timeout(deadline, child.wait()).await {
            Ok(Ok(_status)) => Ok(()),
            Ok(Err(e)) => Err(TransportError::Io(format!("wait for module exit: {e}"))),
            Err(_elapsed) => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                Err(TransportError::Timeout)
            }
        }
    }
}

/// stderr 转发：后台任务逐行读取，带模块 id 前缀转 `tracing::warn!`。
fn spawn_stderr_forwarder(module_id: &str, stderr: tokio::process::ChildStderr) {
    let module_id = module_id.to_string();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => tracing::warn!(module = %module_id, "[stderr] {}", line),
                Ok(None) => break, // EOF：子进程退出（或关闭了 stderr）
                Err(e) => {
                    tracing::debug!(module = %module_id, error = %e, "stderr read ended");
                    break;
                }
            }
        }
    });
}

/// 读错误映射：EOF / 管道破裂 = 对端退出（`Closed`）；其余 = IO 错误。
fn map_read_error(what: &str, e: std::io::Error) -> TransportError {
    match e.kind() {
        ErrorKind::UnexpectedEof | ErrorKind::BrokenPipe | ErrorKind::ConnectionReset => {
            TransportError::Closed
        }
        _ => TransportError::Io(format!("{what}: {e}")),
    }
}

/// 写错误映射：管道破裂 = 对端退出（`Closed`）；其余 = IO 错误。
fn map_write_error(e: std::io::Error) -> TransportError {
    match e.kind() {
        ErrorKind::BrokenPipe | ErrorKind::ConnectionReset | ErrorKind::UnexpectedEof => {
            TransportError::Closed
        }
        _ => TransportError::Io(format!("write frame: {e}")),
    }
}
