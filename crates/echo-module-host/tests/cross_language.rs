//! 跨语言模块（Python）——证明**协议即 ABI**。
//!
//! `examples/minimal-module-py/module.py` 是一个纯 Python 文件，不依赖任何 Rust
//! 代码、不用 SDK，只按「4 字节小端长度前缀 + JSON」收发帧。本测试用**真实的
//! `StdioTransport`**（spawn 子进程、走管道）把它拉起来，断言：
//!
//! 1. 握手成功（`Hello` → `Welcome` → `Register` → `Ready`）；
//! 2. 贡献被宿主正确解析（`Contribution::Service`，即 M4 修过的那个重复键 bug）；
//! 3. `invoke` 往返正确；
//! 4. 未知贡献返回错误结果（而不是崩掉或挂住）；
//! 5. `drain` 能让它优雅退出。
//!
//! 为什么值得一条测试：相对 Node 生态的模块框架，**不锁定语言**是实质优势。
//! 但"协议即 ABI"这句话如果只写在文档里，它就是个说法；跑一遍才算证据。
//!
//! 找不到 `python3` 时**跳过**（不是失败）：CI 的 ubuntu-latest 自带，
//! 但别的环境未必。

use std::path::PathBuf;
use std::time::Duration;

use echo_module_host::api::{
    Contribution, Hello, HostToModule, InvokeContext, InvokeOutcome, ModuleToHost,
};
use echo_module_host::stdio::StdioTransport;
use echo_module_host::supervisor::{ModuleState, ModuleSupervisor};
use echo_module_host::transport::{ModuleSpec, Transport, TransportError};
use serde_json::json;

/// 仓库根（本文件位于 `crates/echo-module-host/tests/`）。
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("repo root")
        .to_path_buf()
}

fn python_module() -> PathBuf {
    repo_root().join("examples/minimal-module-py/module.py")
}

/// `python3` 不可用则跳过（返回 Err 供 `?` 早早退出）。
fn python3() -> Result<PathBuf, String> {
    let output = std::process::Command::new("python3")
        .arg("--version")
        .output()
        .map_err(|e| format!("python3 不可用（跳过）：{e}"))?;
    if !output.status.success() {
        return Err("python3 --version 失败（跳过）".to_string());
    }
    Ok(PathBuf::from("python3"))
}

#[tokio::test]
async fn python_module_speaks_the_protocol() {
    let Ok(python) = python3() else {
        eprintln!("跳过：环境里没有 python3");
        return;
    };
    let script = python_module();
    assert!(
        script.is_file(),
        "Python 模块示例不存在：{}",
        script.display()
    );

    let transport = StdioTransport::new(python).with_args([script.to_string_lossy().to_string()]);
    let supervisor = ModuleSupervisor::new();
    let handle = supervisor
        .start(
            ModuleSpec {
                module_id: "demo.py-greeter".to_string(),
                config: json!({ "written_in": "python" }),
            },
            Box::new(transport),
        )
        .await
        .expect("Python 模块握手应成功（协议名/版本/模块 id 一致）");

    // 1. 握手完成
    assert_eq!(handle.state(), ModuleState::Ready);

    // 2. 贡献被正确解析——这正是 M4 修掉的 ServiceContribution 重复键 bug：
    //    若 wire 字段还叫 `kind`，这里的 `Register` 会整帧解析失败、握手超时。
    let contributions = handle.contributions();
    assert_eq!(contributions.len(), 1, "应注册 1 项贡献：{contributions:?}");
    match &contributions[0] {
        Contribution::Service(service) => {
            assert_eq!(service.key, "greeter");
            assert_eq!(service.kind, "greeter");
            assert_eq!(service.version, 1);
        }
        other => panic!("期望 Service 贡献，实际：{other:?}"),
    }

    // 3. invoke 往返
    let outcome = handle
        .invoke(
            "greeter",
            InvokeContext::default(),
            json!({ "name": "cross-language" }),
        )
        .await;
    match &outcome {
        InvokeOutcome::Ok { text, .. } => {
            assert!(
                text.contains("hello, cross-language"),
                "Python 模块返回内容不符：{text}"
            );
        }
        other => panic!("期望 Ok，实际：{other:?}"),
    }

    // 4. 未知贡献 → 结构化错误，不是崩溃也不是挂住
    let bad = handle
        .invoke("nope", InvokeContext::default(), json!({}))
        .await;
    match &bad {
        InvokeOutcome::Error { code, .. } => assert_eq!(code, "unknown_contribution"),
        other => panic!("期望 Error，实际：{other:?}"),
    }

    // 5. 排空 → 优雅退出
    handle
        .drain(Duration::from_secs(5))
        .await
        .expect("drain 应成功");
    assert_eq!(handle.state(), ModuleState::Stopped);
}

/// 版本不兼容必须**拒绝**，而不是"先连上再说"。
///
/// 这条用 Python 模块做对端，验证的是宿主侧行为：把 `PROTOCOL_VERSION` 对不上的
/// 模块拉起来，`start` 必须返回错误。
#[tokio::test]
async fn python_module_rejects_mismatched_protocol_version() {
    let Ok(python) = python3() else {
        eprintln!("跳过：环境里没有 python3");
        return;
    };
    let script = python_module();
    if !script.is_file() {
        return;
    }

    // 用一个"坏模块"：把协议版本改成 999。用 `-c` 内联生成，不落盘。
    let patch = format!(
        r#"
import sys
src = open({path:?}).read().replace("PROTOCOL_VERSION = 1", "PROTOCOL_VERSION = 999")
exec(compile(src, "module.py", "exec"), {{"__name__": "__main__"}})
"#,
        path = script.to_string_lossy().to_string()
    );

    let transport = StdioTransport::new(python).with_args(["-c", &patch]);
    let supervisor = ModuleSupervisor::new();
    let started = supervisor
        .start(
            ModuleSpec {
                module_id: "demo.py-bad".to_string(),
                config: json!(null),
            },
            Box::new(transport),
        )
        .await;
    let error = match started {
        Err(error) => error,
        Ok(handle) => panic!(
            "协议版本不兼容必须拒绝连接，实际握手成功（state={:?}）",
            handle.state()
        ),
    };

    match error {
        TransportError::Protocol(message) => {
            assert!(
                message.contains("999") || message.to_lowercase().contains("version"),
                "错误信息应指出协议版本问题：{message}"
            );
        }
        other => panic!("期望 Protocol 错误，实际：{other:?}"),
    }
}

/// 纯线上形状自检：Python 模块发出的帧，宿主能逐条解析（不经过 supervisor）。
///
/// 与上面两条的区别：这里不依赖 supervisor 的策略，直接对**帧**做断言，
/// 便于定位「是协议问题还是监督逻辑问题」。
#[tokio::test]
async fn python_module_frames_parse_with_the_rust_types() {
    let Ok(python) = python3() else {
        eprintln!("跳过：环境里没有 python3");
        return;
    };
    let script = python_module();
    if !script.is_file() {
        return;
    }

    let transport = StdioTransport::new(python).with_args([script.to_string_lossy().to_string()]);
    let connection = transport
        .start(ModuleSpec {
            module_id: "demo.py-frames".to_string(),
            config: json!({}),
        })
        .await
        .expect("transport 应能 spawn 并写入 Hello");

    // `Transport::start` 只负责 spawn 出进程并连上管道，**不**发 Hello
    // （握手是 supervisor 的职责）。这里直接对帧操作，所以自己发。
    connection
        .send(HostToModule::Hello(Hello {
            protocol: "echo-module".to_string(),
            version: 1,
            module_id: "demo.py-frames".to_string(),
            config: json!({ "note": "直接对帧，不经过 supervisor" }),
        }))
        .await
        .expect("写入 Hello 应成功");

    // 接下来三条应答必须都能解析成 Rust 类型。
    let welcome = connection.recv().await.expect("应收到 Welcome");
    match welcome {
        ModuleToHost::Welcome(w) => {
            assert_eq!(w.protocol, "echo-module");
            assert_eq!(w.version, 1);
            assert_eq!(w.module_id, "demo.py-frames");
        }
        other => panic!("期望 Welcome，实际：{other:?}"),
    }

    let register = connection.recv().await.expect("应收到 Register");
    match register {
        ModuleToHost::Register(r) => assert_eq!(r.contributions.len(), 1),
        other => panic!("期望 Register，实际：{other:?}"),
    }

    let ready = connection.recv().await.expect("应收到 Ready");
    assert_eq!(ready, ModuleToHost::Ready);

    connection
        .shutdown(Duration::from_secs(5))
        .await
        .expect("shutdown 应成功");
}
