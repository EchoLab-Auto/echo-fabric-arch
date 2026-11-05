//! 测试对端模块（stdio 运输层 conformance 配套，见 `tests/conformance_stdio.rs`）。
//!
//! 一个最小模块进程：从 stdin 读协议帧、向 stdout 回协议帧（4 字节小端长度前缀 + JSON）。
//!
//! 脚本行为：
//! - `Hello` → 回 `Welcome`（回显 protocol / version / module_id）+ `Register`
//!   （注册名为 `echo` 的工具）+ `Ready`；
//! - `Invoke` → 回 `InvokeResult`（文本 = `payload["text"]`，缺省回显整个 payload）；
//!   - `payload = {"show_config": true}` → 回显最近一次 `Hello.config` 的 JSON
//!     （供热替换用例断言「新实例已接管」）；
//!   - `payload = {"crash": true}` → `std::process::exit(1)`（模拟崩溃）；
//!   - `payload = {"hang": true}` → 不回结果（超时 / 取消用例）；
//! - `Cancel` → 回 `Emit` 事件 `peer/cancel`（附 call_id），供宿主侧断言取消已送达；
//! - `Drain` → 回一条 `Log` 后退出（exit 0）；
//! - `Dispose` → 退出；stdin EOF（宿主关闭方向）= 退出。
//!
//! 命令行参数：
//! - `--split-frames`：逐 3 字节分块 + 1ms 间隔慢写（模拟分段到达，
//!   验证宿主侧 `read_exact` 容忍「长度先到、帧体后到」）；
//! - `--stay-alive`：无视 stdin EOF 与 Drain 退出，持续存活
//!   （验证宿主 `shutdown` 超时后的强杀路径）。
//!
//! 环境：若设置 `ECHO_MODULE_TEST_PEER_TAG`，`Ready` 后额外回一条 `Emit`
//! `peer/env`，携带 tag、子进程环境变量个数与 PATH 存在性（供最小环境断言）。
//!
//! 仅依赖 std 与本包既有依赖（echo-module-api / serde_json），无新增第三方依赖。

use std::io::{self, Read, Write};
use std::time::Duration;

use echo_module_api::{
    capabilities, Contribution, Emit, Hello, HostToModule, InvokeOutcome, InvokeResult, LogLevel,
    LogRecord, ModuleToHost, Register, Welcome,
};
use serde_json::{json, Value};

/// 与宿主侧一致的读侧上限（超出视为协议损坏）。
const MAX_FRAME_BYTES: u32 = 16 * 1024 * 1024;

fn main() {
    let split = std::env::args().any(|arg| arg == "--split-frames");
    let stay_alive = std::env::args().any(|arg| arg == "--stay-alive");
    let env_tag = std::env::var("ECHO_MODULE_TEST_PEER_TAG").ok();
    // 最近一次 `Hello` 的 config（`show_config` 回显用）。
    let mut config = Value::Null;

    let stdin = io::stdin();
    let mut reader = stdin.lock();
    let stdout = io::stdout();
    let mut writer = stdout.lock();

    loop {
        let msg = match read_frame(&mut reader) {
            Ok(Some(msg)) => msg,
            // stdin EOF：宿主关停；--stay-alive 时故意无视（供 shutdown 强杀用例）。
            Ok(None) if stay_alive => park_forever(),
            Ok(None) => return,
            Err(e) => {
                eprintln!("test peer: frame error: {e}");
                std::process::exit(2);
            }
        };
        match msg {
            HostToModule::Hello(hello) => {
                config = hello.config.clone();
                on_hello(&mut writer, &hello, split);
                if let Some(tag) = &env_tag {
                    let payload = json!({
                        "tag": tag,
                        "env_count": std::env::vars().count(),
                        "has_path": std::env::var_os("PATH").is_some(),
                    });
                    send(
                        &mut writer,
                        &ModuleToHost::Emit(Emit {
                            event: "peer/env".to_string(),
                            payload,
                        }),
                        split,
                    );
                }
            }
            HostToModule::Invoke(invoke) => {
                if invoke.payload.get("crash").and_then(Value::as_bool) == Some(true) {
                    // 模拟崩溃：不回复、不清理，直接非零退出。
                    std::process::exit(1);
                }
                if invoke.payload.get("hang").and_then(Value::as_bool) == Some(true) {
                    continue; // 挂起：不回结果（等待宿主 Cancel）
                }
                if invoke.payload.get("show_config").and_then(Value::as_bool) == Some(true) {
                    // 回显最近一次 Hello.config：热替换用例据此区分新旧实例。
                    send(
                        &mut writer,
                        &ModuleToHost::InvokeResult(InvokeResult {
                            call_id: invoke.call_id.clone(),
                            outcome: InvokeOutcome::ok(config.to_string()),
                        }),
                        split,
                    );
                    continue;
                }
                let text = invoke
                    .payload
                    .get("text")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| invoke.payload.to_string());
                send(
                    &mut writer,
                    &ModuleToHost::InvokeResult(InvokeResult {
                        call_id: invoke.call_id.clone(),
                        outcome: InvokeOutcome::ok(text),
                    }),
                    split,
                );
            }
            HostToModule::Cancel(cancel) => {
                // 让宿主可观测「Cancel 已送达模块进程」。
                send(
                    &mut writer,
                    &ModuleToHost::Emit(Emit {
                        event: "peer/cancel".to_string(),
                        payload: json!({"call_id": cancel.call_id}),
                    }),
                    split,
                );
            }
            HostToModule::Drain(_drain) => {
                send(
                    &mut writer,
                    &ModuleToHost::Log(LogRecord {
                        level: LogLevel::Info,
                        message: "drain received; exiting".to_string(),
                        fields: None,
                    }),
                    split,
                );
                if stay_alive {
                    continue; // 故意不退出（供 shutdown 强杀用例）
                }
                return;
            }
            HostToModule::Dispose => return,
            HostToModule::Event(_) => {} // 测试模块不订阅事件：忽略
            HostToModule::HostCallResult(_) => {} // 测试模块不发起 HostCall：忽略
        }
    }
}

/// 永远阻塞（`--stay-alive`：无视 stdin EOF / Drain，等待宿主强杀）。
fn park_forever() -> ! {
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

/// 握手应答：Welcome（回显）+ Register（echo 工具）+ Ready。
fn on_hello(writer: &mut impl Write, hello: &Hello, split: bool) {
    send(
        writer,
        &ModuleToHost::Welcome(Welcome {
            protocol: hello.protocol.clone(),
            version: hello.version,
            module_id: hello.module_id.clone(),
            capabilities: vec![
                capabilities::SERVICES.to_string(),
                capabilities::CANCEL.to_string(),
            ],
        }),
        split,
    );
    send(
        writer,
        &ModuleToHost::Register(Register {
            contributions: vec![echo_contribution()],
        }),
        split,
    );
    send(writer, &ModuleToHost::Ready, split);
}

/// 注册的 `echo` 贡献：`kind` 由宿主（这里是测试）自己定义，框架只透传。
///
/// 用 `Custom` 而不是枚举里某个具体变体，正是为了证明**协议不需要认识
/// 业务分类**——夹具想叫什么就叫什么。
fn echo_contribution() -> Contribution {
    Contribution::Custom {
        kind: "host_defined".to_string(),
        name: "echo".to_string(),
        payload: json!({
            "description": "回显 payload（stdio 测试模块）",
            "schema": {
                "type": "object",
                "properties": {
                    "text": {"type": "string"},
                    "show_config": {"type": "boolean"},
                    "crash": {"type": "boolean"},
                    "hang": {"type": "boolean"},
                },
            },
        }),
    }
}

/// 读一帧：4 字节 LE 长度前缀 + JSON 帧体；EOF（干净关闭）= `Ok(None)`。
fn read_frame(reader: &mut impl Read) -> io::Result<Option<HostToModule>> {
    let mut len_bytes = [0u8; 4];
    if !read_exact_or_eof(reader, &mut len_bytes)? {
        return Ok(None);
    }
    let len = u32::from_le_bytes(len_bytes);
    if len > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame length {len} exceeds limit {MAX_FRAME_BYTES}"),
        ));
    }
    let mut body = vec![0u8; len as usize];
    reader.read_exact(&mut body)?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// `read_exact`，但读到 0 字节（EOF）时返回 `false` 而不是错误。
fn read_exact_or_eof(reader: &mut impl Read, buf: &mut [u8]) -> io::Result<bool> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => return Ok(false),
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}

/// 写一帧；`split` = 逐 3 字节分块慢写（分段到达用例）。
fn send(writer: &mut impl Write, msg: &ModuleToHost, split: bool) {
    let Ok(body) = serde_json::to_vec(msg) else {
        std::process::exit(2);
    };
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
    frame.extend_from_slice(&body);

    let result = if split {
        write_split(writer, &frame)
    } else {
        writer.write_all(&frame).and_then(|()| writer.flush())
    };
    if let Err(e) = result {
        eprintln!("test peer: write error: {e}");
        std::process::exit(2);
    }
}

/// 分段慢写：每 3 字节一块、块间 1ms（模拟「长度先到、帧体后到」）。
fn write_split(writer: &mut impl Write, frame: &[u8]) -> io::Result<()> {
    for chunk in frame.chunks(3) {
        writer.write_all(chunk)?;
        writer.flush()?;
        std::thread::sleep(Duration::from_millis(1));
    }
    Ok(())
}
