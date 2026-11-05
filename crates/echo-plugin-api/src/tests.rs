//! 协议契约测试：序列化往返、前向兼容（未知字段容忍）、版本协商。

use crate::contribution::*;
use crate::protocol::*;
use serde_json::json;

fn roundtrip_host(msg: HostToPlugin) {
    let text = serde_json::to_string(&msg).unwrap();
    let back: HostToPlugin = serde_json::from_str(&text).unwrap();
    assert_eq!(msg, back, "HostToPlugin roundtrip: {text}");
}

fn roundtrip_plugin(msg: PluginToHost) {
    let text = serde_json::to_string(&msg).unwrap();
    let back: PluginToHost = serde_json::from_str(&text).unwrap();
    assert_eq!(msg, back, "PluginToHost roundtrip: {text}");
}

#[test]
fn host_messages_roundtrip() {
    roundtrip_host(HostToPlugin::Hello(Hello {
        protocol: PROTOCOL_NAME.into(),
        version: PROTOCOL_VERSION,
        plugin_id: "websearch".into(),
        config: json!({"api_key": "k"}),
    }));
    roundtrip_host(HostToPlugin::Invoke(Invoke {
        call_id: "websearch:1".into(),
        contribution: "web_search".into(),
        ctx: InvokeContext {
            session_id: Some("local:tui::local_user".into()),
            team_id: Some("alix".into()),
            branch_id: None,
            deadline_ms: Some(30_000),
        },
        payload: json!({"query": "rust"}),
    }));
    roundtrip_host(HostToPlugin::Cancel(Cancel {
        call_id: "websearch:1".into(),
    }));
    roundtrip_host(HostToPlugin::Event(EventNotification {
        event: "session/start".into(),
        payload: json!({"id": "s1"}),
    }));
    roundtrip_host(HostToPlugin::Drain(Drain { deadline_ms: 5_000 }));
    roundtrip_host(HostToPlugin::Dispose);
}

#[test]
fn plugin_messages_roundtrip() {
    roundtrip_plugin(PluginToHost::Welcome(Welcome {
        protocol: PROTOCOL_NAME.into(),
        version: PROTOCOL_VERSION,
        plugin_id: "websearch".into(),
        capabilities: vec!["tools".into(), "cancel".into()],
    }));
    roundtrip_plugin(PluginToHost::Register(Register {
        contributions: vec![Contribution::Tool(ToolContribution {
            name: "web_search".into(),
            description: "搜索".into(),
            parameters: json!({"type": "object"}),
            category: "builtin".into(),
            timeout_hint_secs: Some(60),
            package: Some("echo-agent.tools.builtin".into()),
            visible_reply: false,
        })],
    }));
    roundtrip_plugin(PluginToHost::InvokeResult(InvokeResult {
        call_id: "websearch:1".into(),
        outcome: InvokeOutcome::Ok {
            text: "ok".into(),
            images: vec![],
            result: serde_json::json!({"structured": true}),
        },
    }));
    roundtrip_plugin(PluginToHost::InvokeResult(InvokeResult {
        call_id: "websearch:2".into(),
        outcome: InvokeOutcome::Error {
            code: "timeout".into(),
            message: "deadline".into(),
        },
    }));
    roundtrip_plugin(PluginToHost::Emit(Emit {
        event: "tool/result".into(),
        payload: json!({}),
    }));
    roundtrip_plugin(PluginToHost::Log(LogRecord {
        level: LogLevel::Warn,
        message: "slow upstream".into(),
        fields: None,
    }));
    roundtrip_plugin(PluginToHost::Ready);
    roundtrip_plugin(PluginToHost::Failed(Failure {
        code: "init".into(),
        message: "boom".into(),
    }));
}

#[test]
fn unknown_fields_are_tolerated() {
    // 前向兼容：新端点发的帧，旧端点必须能解码（未知字段忽略）。
    let frame = json!({
        "type": "invoke",
        "payload": {
            "call_id": "p:1",
            "contribution": "t",
            "future_field": {"nested": true},
            "ctx": {"session_id": "s", "another_new_field": 1},
            "payload": {}
        }
    });
    let msg: HostToPlugin = serde_json::from_value(frame).expect("unknown fields tolerated");
    match msg {
        HostToPlugin::Invoke(inv) => {
            assert_eq!(inv.call_id, "p:1");
            assert_eq!(inv.ctx.session_id.as_deref(), Some("s"));
        }
        other => panic!("expected Invoke, got {other:?}"),
    }
}

#[test]
fn invoke_outcome_result_field_roundtrip_and_backcompat() {
    // W1.1：结构化结果字段（插件实例 op 返回值的协议载体）。
    // ① 带 result 的帧往返；
    roundtrip_plugin(PluginToHost::InvokeResult(InvokeResult {
        call_id: "adapter:1".into(),
        outcome: InvokeOutcome::Ok {
            text: String::new(),
            images: vec![],
            result: json!({"state": "connected", "self_id": "123"}),
        },
    }));

    // ② 旧插件（不带 result 字段的旧帧）→ 缺省 null（向后兼容解码）；
    let legacy = json!({
        "type": "invoke_result",
        "payload": {
            "call_id": "p:1",
            "outcome": {"status": "ok", "text": "legacy"}
        }
    });
    let msg: PluginToHost = serde_json::from_value(legacy).expect("legacy frame tolerated");
    match msg {
        PluginToHost::InvokeResult(r) => match r.outcome {
            InvokeOutcome::Ok { text, result, .. } => {
                assert_eq!(text, "legacy");
                assert_eq!(result, serde_json::Value::Null);
            }
            other => panic!("expected Ok, got {other:?}"),
        },
        other => panic!("expected InvokeResult, got {other:?}"),
    }

    // ③ result 为 null 时线上省略（旧端点解码无差异）。
    let plain = serde_json::to_string(&PluginToHost::InvokeResult(InvokeResult {
        call_id: "p:2".into(),
        outcome: InvokeOutcome::ok("plain"),
    }))
    .unwrap();
    assert!(
        !plain.contains("\"result\""),
        "null result must be omitted on the wire: {plain}"
    );
}

#[test]
fn unknown_enum_variant_is_rejected_cleanly() {
    // 未知变体：解码失败（可读错误）而非 panic——宿主据此走版本不兼容路径。
    let frame = json!({"type": "future_message", "payload": {}});
    assert!(serde_json::from_value::<HostToPlugin>(frame).is_err());
}

#[test]
fn version_negotiation() {
    assert!(compatible(PROTOCOL_VERSION, PROTOCOL_VERSION));
    assert!(!compatible(1, 2));
    assert!(!compatible(2, 1));
}

#[test]
fn invoke_context_defaults_and_omission() {
    let ctx: InvokeContext = serde_json::from_value(json!({})).unwrap();
    assert_eq!(ctx, InvokeContext::default());
    // 空的 ctx 字段在线上省略（skip_serializing_if）
    let inv = Invoke {
        call_id: "a".into(),
        contribution: "t".into(),
        ctx: InvokeContext::default(),
        payload: json!(null),
    };
    let text = serde_json::to_string(&inv).unwrap();
    assert!(!text.contains("session_id"));
    assert!(!text.contains("team_id"));
}

#[test]
fn instance_contribution_roundtrip_and_capabilities() {
    // P1（实例插件化）：InstanceContribution 线上形态（贡献 + 能力位 +
    // 词汇结构）。
    let instance = Contribution::PluginInstance(InstanceContribution {
        name: "qq".into(),
        platform: "qq".into(),
        display_name: "QQ / OneBot".into(),
        capabilities: InstanceCapabilities {
            send_text: true,
            send_media: true,
            groups: true,
            friends: true,
            gating: true,
            login: true,
        },
        config_schema: None,
        ui: Some(json!({"version": 1, "panels": []})),
        session_labels: Some(json!({"dm": "私聊", "group": "群"})),
        tools: Vec::new(),
    });
    let text = serde_json::to_string(&instance).unwrap();
    let back: Contribution = serde_json::from_str(&text).unwrap();
    assert_eq!(instance, back);

    // 缺省能力位全 false（fail-closed：宿主只调用声明过的方法）。
    let caps: InstanceCapabilities = serde_json::from_value(json!({})).expect("defaults");
    assert!(!caps.send_text && !caps.gating && !caps.login);

    // 注：`MessageTarget` / `ChannelType` / `IncomingMessage`（QQ 出站寻址与
    // 入站消息）曾在 v0.13 之前定义于此 crate。那是从 EchoAgentCore 带过来的
    // 聊天平台词汇，**零使用者**——EchoAgentCore 用自己的 `echo-defs::chat`
    // 副本，本仓内部也只有这个测试在测它们的 serde 往返。已删除。
}

/// wire tag 冻结：`Contribution::PluginInstance` 在线上必须仍是 `adapter`。
///
/// 宿主与插件是两个可独立升级的进程；改这个字符串会同时打断「新 Core +
/// 已部署旧插件」与「旧 Core + 新插件」两个方向。Rust 侧改名（`Adapter` →
/// `PluginInstance`）是内部可读性，线上一字节不动。
#[test]
fn plugin_instance_contribution_wire_tag_is_frozen() {
    let value = serde_json::to_value(Contribution::PluginInstance(InstanceContribution {
        name: "qq".into(),
        platform: "qq".into(),
        display_name: "QQ".into(),
        capabilities: InstanceCapabilities::default(),
        config_schema: None,
        ui: None,
        session_labels: None,
        tools: Vec::new(),
    }))
    .unwrap();
    assert_eq!(value["kind"], serde_json::json!("adapter"), "{value}");
    // 别名也接受（未来换 tag 的过渡窗口）。
    let back: Contribution = serde_json::from_value(serde_json::json!({
        "kind": "plugin_instance",
        "name": "qq",
        "platform": "qq",
        "display_name": "QQ"
    }))
    .unwrap();
    assert!(matches!(back, Contribution::PluginInstance(_)));
}

#[test]
fn contribution_shapes() {
    let tool = Contribution::Tool(ToolContribution {
        name: "bash".into(),
        description: "d".into(),
        parameters: json!({}),
        category: "builtin".into(),
        timeout_hint_secs: None,
        package: None,
        visible_reply: false,
    });
    let text = serde_json::to_string(&tool).unwrap();
    let back: Contribution = serde_json::from_str(&text).unwrap();
    assert_eq!(tool, back);
    // 最小字段集：缺省 category 回退 "plugin"，parameters 回退 null
    let minimal: ToolContribution =
        serde_json::from_value(json!({"name": "t", "description": "d"})).unwrap();
    assert_eq!(minimal.category, "plugin");
    assert_eq!(minimal.parameters, json!(null));
}

/// **每个贡献变体都必须能过一遍 JSON**。
///
/// 回归背景（2026-10-19 M4 最小宿主撞出）：`ServiceContribution` 曾把载荷
/// 字段也命名成 `kind`，而 `Contribution` 是内部标签枚举、tag 同为 `kind`——
/// 序列化结果是 `{"kind":"service","key":"…","kind":"…"}`，**重复键**。
/// 宿主侧 serde_json 报 `duplicate field`，整帧解析失败。
///
/// 为什么长期没被发现：inproc 轨类型直连、零序列化，这个冲突不出现；而当时
/// 唯一声明 `Service` 贡献的插件跑在 inproc 上。直到有示例同时用
/// 「Service 贡献 + stdio 轨」才暴露。
///
/// 这条测试把「所有变体都能过 JSON」变成硬约束，避免再出现某变体只在
/// 单条轨上可用。
#[test]
fn every_contribution_variant_roundtrips_through_json() {
    use crate::contribution::InstanceCapabilities;

    let variants = vec![
        Contribution::Tool(ToolContribution {
            name: "bash".into(),
            description: "run a command".into(),
            parameters: json!({ "type": "object" }),
            category: "builtin".into(),
            timeout_hint_secs: Some(30),
            package: Some("acme.pkg".into()),
            visible_reply: true,
        }),
        Contribution::Skill(SkillContribution {
            name: "deploy".into(),
            description: "how to deploy".into(),
            path: Some("/skills/deploy".into()),
        }),
        Contribution::Service(ServiceContribution {
            key: "greeter".into(),
            kind: "greeter".into(),
            version: 3,
        }),
        Contribution::Events(EventContribution {
            subscribe: vec!["tick".into(), "tock".into()],
        }),
        Contribution::PluginInstance(InstanceContribution {
            name: "demo".into(),
            platform: "demo".into(),
            display_name: "Demo".into(),
            capabilities: InstanceCapabilities::default(),
            config_schema: None,
            ui: None,
            session_labels: None,
            tools: vec![],
        }),
    ];

    for contribution in variants {
        let text = serde_json::to_string(&contribution)
            .unwrap_or_else(|e| panic!("序列化失败 {contribution:?}: {e}"));
        let back: Contribution = serde_json::from_str(&text).unwrap_or_else(|e| {
            panic!("反序列化失败（wire 形状自相矛盾？）\n  json: {text}\n  err: {e}")
        });
        assert_eq!(contribution, back, "roundtrip 不一致：{text}");
    }
}

/// `ServiceContribution` 的 wire 字段是 `service_kind`——与枚举 tag `kind` 不撞。
#[test]
fn service_contribution_wire_field_avoids_tag_collision() {
    let service = Contribution::Service(ServiceContribution {
        key: "greeter".into(),
        kind: "greeter".into(),
        version: 1,
    });
    let value = serde_json::to_value(&service).unwrap();
    let object = value.as_object().expect("贡献应序列化为对象");

    assert_eq!(object.get("kind").and_then(|v| v.as_str()), Some("service"));
    assert_eq!(
        object.get("service_kind").and_then(|v| v.as_str()),
        Some("greeter"),
        "载荷字段必须改名，否则与枚举 tag 重复"
    );
    // 兜底：序列化文本里 `"kind"` 只应出现一次（重复键会让宿主解析失败）。
    let text = serde_json::to_string(&service).unwrap();
    assert_eq!(
        text.matches("\"kind\"").count(),
        1,
        "JSON 出现重复键：{text}"
    );
}
