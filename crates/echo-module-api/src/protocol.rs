//! 协议消息（Host ↔ Module）与版本协商。

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::contribution::Contribution;

/// 协议名（Hello/Welcome 握手比对；防串线）。
pub const PROTOCOL_NAME: &str = "echo-module";

/// 当前协议主版本。Hello 握手时比对：不一致拒绝连接。
///
/// 变更规则：只增字段/只增变体 = 不 bump；语义变更/移除字段 = bump 并同时
/// 更新全部运输层与 conformance。
pub const PROTOCOL_VERSION: u32 = 1;

/// 版本兼容判定：主版本一致即可（v1 阶段即严格相等）。
pub fn compatible(host: u32, module: u32) -> bool {
    host == module
}

// ── Host → Module ─────────────────────────────────────────────────────────

/// 宿主 → 模块消息。线上编码为 adjacently-tagged：`{"type": …, "payload": …}`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum HostToModule {
    /// 握手第一步：宣告协议版本与模块配置。
    Hello(Hello),
    /// 调用一个贡献（工具调用 / 服务方法）。
    Invoke(Invoke),
    /// 取消在途调用（尽力送达；对端可能已完成）。
    Cancel(Cancel),
    /// 宿主 → 模块事件通知（仅已订阅的事件会送达）。
    Event(EventNotification),
    /// 排空：不再接受新 Invoke，等待在途完成后退出（deadline 后强杀）。
    Drain(Drain),
    /// 终止：立即释放并退出（宿主已确保无在途）。
    Dispose,
    /// 对模块 `HostCall` 的回执（`call_id` 由模块生成、原样带回）。
    HostCallResult(HostCallResult),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol: String,
    pub version: u32,
    /// 宿主侧的模块实例 id（与配置行 id 一致）。
    pub module_id: String,
    /// 模块配置（行 config 原样透传）。
    #[serde(default)]
    pub config: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Invoke {
    /// 调用 id：`<module_id>:<序号>`，配对 InvokeResult / Cancel。
    pub call_id: String,
    /// 目标贡献名（工具名 / 服务方法名）。
    pub contribution: String,
    /// 调用上下文（会话归属等；替代旧的参数注入 `__session_id` / `__team_id`）。
    #[serde(default)]
    pub ctx: InvokeContext,
    /// 调用参数（JSON Schema 由贡献注册时的 schema 声明）。
    #[serde(default)]
    pub payload: Value,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct InvokeContext {
    /// 归属会话 id（宿主自定义语义；框架只透传）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// 归属分组 id（宿主自定义语义；框架只透传）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team_id: Option<String>,
    /// 归属回复分支（临时分支可见性）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch_id: Option<String>,
    /// 剩余时限（毫秒）；None = 由宿主外层守卫兜底。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cancel {
    pub call_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventNotification {
    pub event: String,
    #[serde(default)]
    pub payload: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Drain {
    /// 排空期限（毫秒）；超时后宿主强杀（子进程 SIGKILL）。
    pub deadline_ms: u64,
}

// ── Module → Host ─────────────────────────────────────────────────────────

/// 模块 → 宿主消息。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum ModuleToHost {
    /// 握手应答（协议版本 / 能力）。
    Welcome(Welcome),
    /// 贡献注册（可多次；每次**整体替换**该模块的贡献集合——绝不保留部分集合）。
    Register(Register),
    /// 调用结果（终态；之后该 call_id 的消息一律忽略）。
    InvokeResult(InvokeResult),
    /// 模块 → 宿主事件（宿主按订阅路由；subagent 完成回灌等）。
    Emit(Emit),
    /// 日志（宿主转发到 tracing，带模块前缀）。
    Log(LogRecord),
    /// 初始化完成（Welcome 后、可选 Register 后）。
    Ready,
    /// 初始化失败（宿主按策略处理：报错 / 重启 / 熔断）。
    Failed(Failure),
    /// 模块 → 宿主服务回呼（宿主按注册表路由，结果回执为 `HostCallResult`）。
    HostCall(HostCall),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Welcome {
    pub protocol: String,
    pub version: u32,
    /// 模块自报 id（与 Hello.module_id 应一致；不一致宿主拒绝）。
    pub module_id: String,
    /// 能力 / feature 列表（如 `"tools"`、`"events"`、`"cancel"`）；双方都声明才可用。
    #[serde(default)]
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Register {
    pub contributions: Vec<Contribution>,
    /// 模块**自述身份**（可选）。
    ///
    /// 缺省 = 由宿主按配置行合成一个（id / 命令 / 运输形态）。那种合成只够
    /// 让面板显示一行——`kind` 只能是运输拼写（`stdio`），`package` /
    /// `requires` / `provides` 全空，于是宿主拿不到任何"这模块该归谁管"
    /// 的信息：宿主按 `kind` 派生作用域门控（未知 kind fail-closed 到进程级），
    /// 包级启停与依赖排序也无从谈起。
    ///
    /// 声明后，宿主用这份 claim 造描述符（**运行形态 `runtime` 与来源
    /// `builtin` 仍由宿主填**——模块看不见自己是被 stdio 还是 dylib 装进来的，
    /// 也不该知道宿主把它算不算"内置"）。
    ///
    /// 兼容：v1 协议只增字段，旧模块不发这个字段（`None`）行为不变。
    ///
    /// **为什么是 `Box`**：整条 `ModuleToHost` 是个 enum，内联一份 claim 会把
    /// 每一帧（含热路径上的 `InvokeResult`）的枚举尺寸撑到 300 字节量级
    /// （clippy `large_enum_variant` 当场报警）。身份是「每次注册一次」的载荷，
    /// 一次额外分配换帧尺寸，值。构造侧用 `Some(Box::new(claim))`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest: Option<Box<ModuleManifestClaim>>,
}

/// 模块在协议上自述的身份——[`crate::ModuleManifest`] 的线格式。
///
/// 与宿主侧清单的差别只有两个字段，且都是**宿主决定**的：
///
/// | 字段 | 为什么不由模块声明 |
/// | --- | --- |
/// | `runtime` | 同一个模块可以被 stdio 或 dylib 装进来；模块不知道也不该猜 |
/// | `builtin` | 来源维度（编译进宿主 / 目录发现），只有宿主知道 |
///
/// `id` 必须与 `Welcome.module_id` 一致，不一致宿主拒绝连接（防止一个进程
/// 冒充别的模块 id 注册贡献）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModuleManifestClaim {
    /// 稳定唯一 id（`{vendor}.{group}.{name}`）。
    pub id: String,
    /// 人类可读名（面板标题）。
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub description: String,
    /// 能力类别——**开放标签**，词汇表归宿主。宿主据它派生作用域门控。
    #[serde(default)]
    pub kind: String,
    /// 来源标签（宿主用来分组展示）。
    #[serde(default)]
    pub tag: String,
    #[serde(default)]
    pub author: String,
    /// 所属包 id：包级门控（禁一个包 = 其全部成员不可见）按它聚合。
    /// `None` = 本模块自身即包根。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
    /// 硬依赖的宿主服务键：挂载前必须已在宿主服务表里。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires: Vec<String>,
    /// 软依赖（只声明，不阻止挂载）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub optional_requires: Vec<String>,
    /// 本模块提供到宿主服务表的键。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provides: Vec<String>,
}

impl ModuleManifestClaim {
    /// 最小可用身份：只有 id 与类别。
    pub fn new(id: impl Into<String>, kind: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            kind: kind.into(),
            ..Self::default()
        }
    }

    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    pub fn with_version(mut self, version: impl Into<String>) -> Self {
        self.version = version.into();
        self
    }

    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }

    pub fn with_tag(mut self, tag: impl Into<String>) -> Self {
        self.tag = tag.into();
        self
    }

    pub fn with_author(mut self, author: impl Into<String>) -> Self {
        self.author = author.into();
        self
    }

    pub fn with_package(mut self, package: impl Into<String>) -> Self {
        self.package = Some(package.into());
        self
    }

    pub fn with_requires<I, S>(mut self, keys: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.requires = keys.into_iter().map(Into::into).collect();
        self
    }

    pub fn with_provides<I, S>(mut self, keys: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.provides = keys.into_iter().map(Into::into).collect();
        self
    }

    /// 本模块所属的包 id：显式声明优先，未声明时回退为模块 id。
    pub fn package_id(&self) -> &str {
        self.package.as_deref().unwrap_or(&self.id)
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InvokeResult {
    pub call_id: String,
    pub outcome: InvokeOutcome,
}

/// 调用终态：成功（文本 + 可选图片 + 可选结构化结果）或错误。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum InvokeOutcome {
    Ok {
        /// **遗留字段**（文本 / 多模态宿主场景）。新代码请用
        /// [`InvokeOutcome::data`]，只填 `result`。
        ///
        /// 保留是为了旧模块仍能解码：删字段会破坏线上兼容，而去业务化
        /// （把一个泛化的 RPC 结果收窄成单一载荷）需要独立的兼容窗口。
        #[serde(default)]
        text: String,
        /// **遗留字段**（图片 URL / data URI）。同上：新代码请用
        /// [`InvokeOutcome::data`]。
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<String>,
        /// 结构化结果（W1.1，2026-10）：模块实例 op（`custom`）等场景的返回载荷经
        /// 此承载——`status` / 目录 / 二维码等结构化返回不再 JSON 塞 `text`。
        /// 工具面消费者可忽略；缺省 `null`（旧模块不带该字段，向后兼容）。
        #[serde(default, skip_serializing_if = "Value::is_null")]
        result: Value,
    },
    Error {
        /// 机器可读错误码（如 `timeout` / `cancelled` / `module_crashed` / `internal`）。
        code: String,
        message: String,
    },
}

impl InvokeOutcome {
    /// `Ok` 便捷构造：只填遗留的 `text`（无图片、无结构化结果）。
    ///
    /// 新代码优先用 [`data`](Self::data)——语义化结果（状态、目录、文档）
    /// 走 `result` 比塞进字符串更干净，也不必假装它是"文本"。
    pub fn ok(text: impl Into<String>) -> Self {
        Self::Ok {
            text: text.into(),
            images: Vec::new(),
            result: Value::Null,
        }
    }

    /// `Ok` 便捷构造：只填结构化结果 `result`（`text` / `images` 留空）。
    ///
    /// 这是**与领域无关**的返回形状：宿主拿到 `Value` 后自己解释，框架不预设
    /// 它是一段文本、一张图还是一份状态。模块声明服务、宿主调用它、拿回一份
    /// JSON——就这样。
    pub fn data(result: Value) -> Self {
        Self::Ok {
            text: String::new(),
            images: Vec::new(),
            result,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Emit {
    pub event: String,
    #[serde(default)]
    pub payload: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogRecord {
    pub level: LogLevel,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fields: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Failure {
    pub code: String,
    pub message: String,
}

/// 模块 → 宿主服务回呼（`ModuleToHost::HostCall` 载荷；P2 新增）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostCall {
    /// 调用 id：模块生成、配对结果用（约定 `"host:<序号>"`）。
    pub call_id: String,
    /// 宿主注册表键（如 `"sanitizer"`）。
    pub service: String,
    /// 服务方法名（如 `"redact"` / `"scan"`）。
    pub method: String,
    #[serde(default)]
    pub payload: Value,
}

/// 宿主对 [`HostCall`] 的回执（`HostToModule::HostCallResult` 载荷）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostCallResult {
    /// 对应 [`HostCall::call_id`]（模块生成、原样带回）。
    pub call_id: String,
    pub outcome: HostCallOutcome,
}

/// 服务回呼终态：成功（JSON 结果）或错误。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum HostCallOutcome {
    Ok {
        #[serde(default)]
        result: Value,
    },
    Error {
        /// 机器可读错误码（如 `service_not_found` / `method_not_found` / `timeout`）。
        code: String,
        message: String,
    },
}
