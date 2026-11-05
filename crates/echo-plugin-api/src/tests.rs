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
            session_id: Some("scope-1".into()),
            team_id: Some("group-a".into()),
            branch_id: None,
            deadline_ms: Some(30_000),
        },
        payload: json!({"query": "rust"}),
    }));
    roundtrip_host(HostToPlugin::Cancel(Cancel {
        call_id: "websearch:1".into(),
    }));
    roundtrip_host(HostToPlugin::Event(EventNotification {
        event: "resource/created".into(),
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
        capabilities: vec!["services".into(), "cancel".into()],
    }));
    roundtrip_plugin(PluginToHost::Register(Register {
        contributions: vec![
            Contribution::Service(ServiceContribution {
                key: "greeter".into(),
                kind: "greeter".into(),
                version: 1,
            }),
            Contribution::Custom {
                kind: "host_defined".into(),
                name: "some_op".into(),
                payload: json!({"shape": "由宿主与插件自行约定"}),
            },
        ],
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
        event: "op/finished".into(),
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
fn custom_contribution_carries_host_defined_shape() {
    // 领域词汇的唯一入口：宿主自带分类，框架只搬 kind + payload。
    let custom = Contribution::Custom {
        kind: "whatever_the_host_calls_it".into(),
        name: "some_op".into(),
        payload: json!({ "shape": "由宿主与插件自行约定", "n": 1 }),
    };
    let text = serde_json::to_string(&custom).unwrap();
    let back: Contribution = serde_json::from_str(&text).unwrap();
    assert_eq!(custom, back);

    // 线上标签是 `custom`，分类字段必须改名（`kind` 已被枚举 tag 占用）。
    let value = serde_json::to_value(&custom).unwrap();
    assert_eq!(value["kind"], json!("custom"), "{value}");
    assert_eq!(value["custom_kind"], json!("whatever_the_host_calls_it"));
    let object = value.as_object().expect("对象");
    assert_eq!(
        text.matches("\"kind\"").count(),
        1,
        "载荷字段不能叫 kind，否则与枚举 tag 重复：{text}"
    );
    assert!(object.contains_key("custom_kind"));

    // 最小形态：只给分类，name / payload 都可省。
    let minimal: Contribution = serde_json::from_value(json!({
        "kind": "custom",
        "custom_kind": "anything"
    }))
    .expect("最小形态应可解码");
    assert_eq!(minimal.custom_kind(), Some("anything"));
    assert_eq!(minimal.target_name(), None, "没有 name 就不参与寻址");
}

#[test]
fn contribution_shapes() {
    let service = Contribution::Service(ServiceContribution {
        key: "k".into(),
        kind: "".into(),
        version: 0,
    });
    let text = serde_json::to_string(&service).unwrap();
    let back: Contribution = serde_json::from_str(&text).unwrap();
    assert_eq!(service, back);
    // 最小字段集：kind / version 都可省。
    let minimal: ServiceContribution = serde_json::from_value(json!({"key": "k"})).unwrap();
    assert_eq!(minimal.kind, "");
    assert_eq!(minimal.version, 0);
}

#[test]
fn target_name_covers_exactly_the_addressable_variants() {
    let cases: Vec<(Contribution, Option<&str>)> = vec![
        (
            Contribution::Service(ServiceContribution {
                key: "svc".into(),
                kind: String::new(),
                version: 1,
            }),
            Some("svc"),
        ),
        (
            Contribution::Command(CommandContribution {
                name: "do_thing".into(),
                description: String::new(),
            }),
            Some("do_thing"),
        ),
        (
            Contribution::Custom {
                kind: "k".into(),
                name: "op".into(),
                payload: json!(null),
            },
            Some("op"),
        ),
        (
            Contribution::Custom {
                kind: "k".into(),
                name: String::new(),
                payload: json!(null),
            },
            None,
        ),
        (
            Contribution::Text(TextBlockContribution {
                key: "blk".into(),
                label: String::new(),
                order: 0,
            }),
            None,
            // 文本块是"宿主来取"而不是"宿主来调"，所以不参与寻址。
        ),
        (Contribution::Events(EventContribution::default()), None),
    ];
    for (contribution, expected) in cases {
        assert_eq!(
            contribution.target_name(),
            expected,
            "寻址名不符：{contribution:?}"
        );
    }
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
    let variants = vec![
        Contribution::Service(ServiceContribution {
            key: "greeter".into(),
            kind: "greeter".into(),
            version: 3,
        }),
        Contribution::Events(EventContribution {
            subscribe: vec!["tick".into(), "tock".into()],
        }),
        Contribution::Command(CommandContribution {
            name: "do_thing".into(),
            description: "处理某类调用".into(),
        }),
        Contribution::Text(TextBlockContribution {
            key: "plugin:block".into(),
            label: "标题".into(),
            order: 5,
        }),
        Contribution::Custom {
            kind: "host_defined".into(),
            name: "some_op".into(),
            payload: json!({ "arbitrary": ["shape", 1] }),
        },
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
