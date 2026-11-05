//! 贡献类型：插件向宿主注册的能力（工具 / 技能 / 服务 / 事件订阅）。

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 一次注册的贡献。`Register` 消息携带全量集合（整体替换语义）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Contribution {
    Tool(ToolContribution),
    Skill(SkillContribution),
    Service(ServiceContribution),
    Events(EventContribution),
    /// 插件实例贡献（P1，2026-10 实例插件化）：平台插件作为插件交付。
    /// 宿主把 `RemoteInstance`（`echo_defs::plugin_instance::PluginInstance` 的远程实现）
    /// 注册进 `InstanceRegistry`；动作经 `Invoke("<name>:<op>")`，入站经
    /// `HostCall` → 宿主服务 `core_api`（见 `document/design/instance-plugin.md`）。
    ///
    /// **wire tag 保持 `adapter`**：这是宿主与插件之间唯一的契约字符串，
    /// 两侧独立升级（新 Core + 已部署旧插件、旧 Core + 新插件）都依赖它不变。
    /// Rust 侧名字去掉 adapter 是内部可读性，线上一字节不动；`plugin_instance`
    /// 作为别名接受，未来若要换 tag 有过渡窗口。
    #[serde(rename = "adapter", alias = "plugin_instance")]
    PluginInstance(InstanceContribution),
    /// **命令处理**贡献（v0.8.0）：插件声明"这些命令我来处理"。
    ///
    /// 宿主原本只能靠一个封闭 `enum` + 巨型 `match` 分发命令（EchoAgent 里
    /// 是 66 个变体 / 2,068 行）。有了它，"某个命令域归某个插件"就从**硬编码
    /// 分支**变成**注册关系**——插件不在，命令自然无人处理，宿主回明确错误。
    ///
    /// 框架只搬 `name`：命令名是**宿主词汇**（如 `RequestWorkspaceSessions`），
    /// 框架不认识也不解释。
    Command(CommandContribution),
    /// **提示词块**贡献（v0.8.0）：插件声明要向宿主的"文本装配"过程贡献一段
    /// 文本（带顺序与标题）。
    ///
    /// 宿主原本只能给装配函数加参数（"收集上下文段落"这类函数每来一个要注入
    /// 内容的插件就改一次签名）。有了它，装配变成"收集已注册的块 + 排序"，
    /// 与插件数量解耦。
    ///
    /// 框架只搬 `key`（槽内唯一），`label` / `text` 由 [`PluginTextBlock`]
    /// 在运行期给出——**框架不知道"提示词"是什么**，它只知道"有序文本块"。
    ///
    /// [`PluginTextBlock`]: echo_plugin::PluginTextBlock
    Text(TextBlockContribution),
}

impl Contribution {
    /// `Invoke` 的寻址目标；`None` = 这条贡献不是"被调用的"。
    ///
    /// 三种寻址形态：按 `name`（两处）、按 `key`（服务）、按实例名。哪些参与
    /// 寻址是**协议的知识**——放在变体定义旁边，SDK 与宿主都不必各自 match
    /// 一遍具体变体（那会让它们也被迫认识 `Tool` / `Skill` 这些词）。
    pub fn target_name(&self) -> Option<&str> {
        match self {
            Contribution::Tool(tool) => Some(&tool.name),
            Contribution::Skill(skill) => Some(&skill.name),
            Contribution::Service(service) => Some(&service.key),
            Contribution::Events(_) => None,
            // 插件实例的寻址名 = 实例名（宿主按 `Invoke("<name>:<op>")` 寻址）。
            Contribution::PluginInstance(instance) => Some(&instance.name),
            // 命令处理：宿主按 `Invoke("<command>")` 下发。
            Contribution::Command(command) => Some(&command.name),
            // 文本块是"宿主来取"而不是"宿主来调"（拉模式），不参与寻址。
            Contribution::Text(_) => None,
        }
    }
}

fn default_category() -> String {
    "plugin".to_string()
}

/// 工具贡献（遗留领域分类；参数用 JSON Schema 描述）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolContribution {
    pub name: String,
    pub description: String,
    /// 参数 JSON Schema（与注册进 ToolRegistry 的完全一致）。
    #[serde(default)]
    pub parameters: Value,
    /// UI 分组分类（缺省 `"plugin"`）。
    #[serde(default = "default_category")]
    pub category: String,
    /// 工具自声明超时上限（秒）；宿主守卫取其与配置 base 的较大者 + 宽限。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_hint_secs: Option<u64>,
    /// 所属包 id（宿主按包批量勾选用；如 `vendor.suite.feature`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
    /// 成功调用是否即产生平台可见输出（宿主据此抑制「临时等待回复」）。
    #[serde(default)]
    pub visible_reply: bool,
}

/// 技能贡献（SKILL.md 目录或内联描述）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkillContribution {
    pub name: String,
    pub description: String,
    /// 技能目录（含 SKILL.md）；None = 由插件自行提供技能内容（后续扩展）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

/// 服务贡献：以稳定键注册的服务（供 Ctx 服务定位消费）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServiceContribution {
    /// 服务键（`Ctx` 命名空间；与 `ServiceKey::name()` 对应）。
    pub key: String,
    /// 服务类别——**宿主自定义词汇**，框架只透传（如 `"greeter"` / `"loop"`）。
    ///
    /// **wire 名是 `service_kind`，不是 `kind`**：`Contribution` 是内部标签
    /// 枚举且 tag 就叫 `kind`（`{"kind":"service", …}`），载荷字段若也叫 `kind`
    /// 会序列化出**重复键**：
    ///
    /// ```text
    /// {"kind":"service","key":"greeter","kind":"greeter","version":1}
    ///                                          ^^^^ 重复
    /// ```
    ///
    /// 宿主侧 `serde_json` 直接报 `duplicate field`，整帧解析失败。
    ///
    /// 这个 bug 在 **inproc 轨（类型直连、零序列化）完全不可见**——只有走
    /// stdio 才暴露。2026-10-19 M4 的最小宿主示例（唯一一个同时用
    /// `Service` 贡献 + stdio 轨的组合）把它撞了出来。
    ///
    /// 改名零兼容成本：此前这种帧**根本解析不了**，不可能有在用的一端。
    #[serde(rename = "service_kind", default)]
    pub kind: String,
    /// 服务接口版本（消费端据此校验兼容）。
    #[serde(default)]
    pub version: u32,
}

/// 事件订阅：插件希望收到的宿主事件名列表（宿主按名单路由 Event 消息）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventContribution {
    #[serde(default)]
    pub subscribe: Vec<String>,
}

/// 命令处理声明（`Contribution::Command`）。
///
/// 只有名字：**处理逻辑在插件侧**（inproc 走 `Plugin::handle_command`，stdio
/// 走 `Invoke("<name>")`）。框架不搬载荷类型、不做 schema 校验——那都是宿主
/// 与插件之间的契约。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandContribution {
    /// 命令名（宿主词汇；同一个名字只应有一个处理者）。
    pub name: String,
    /// 人读说明（面板展示用；空 = 只显示名字）。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
}

/// 提示词块声明（`Contribution::Text`）。
///
/// 与 [`CommandContribution`] 同款：这里只有**身份与顺序**，文本在运行期由
/// `Plugin::text_blocks(context)` 按需产生（同一个插件可能因会话不同而给出
/// 不同内容）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextBlockContribution {
    /// 块键（宿主装配时用于去重/覆盖；建议 `<插件>:<块名>`）。
    pub key: String,
    /// 面板展示用的标题（空 = 用 `key`）。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub label: String,
    /// 排序权重（小在前；等值按注册序）。
    #[serde(default)]
    pub order: i32,
}

/// 插件实例贡献（P1，2026-10 实例插件化）：宿主把 `RemoteInstance` 注册进
/// `InstanceRegistry`；`capabilities` 声明该插件实例支持的管理动作位（宿主
/// 只调用声明过的方法，未声明的调用拒绝为 `unsupported`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstanceContribution {
    /// 实例名（= Invoke 前缀；全局唯一）。
    pub name: String,
    /// 平台标识（会话作用域 / 显示；如 `"qq"`）。
    pub platform: String,
    /// 展示名（如 `"QQ / OneBot"`）。
    pub display_name: String,
    /// 能力位（`InstanceCapabilities`；宿主 fail-closed 校验）。
    #[serde(default)]
    pub capabilities: InstanceCapabilities,
    /// 插件配置 JSON Schema（校验 / 未来面板表单，可选）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_schema: Option<Value>,
    /// UI 描述符（插件声明的插件实例管理界面；Panel 经通用渲染器呈现；
    /// 结构见 `document/design/instance-plugin.md` §Panel UI 解耦，可选）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui: Option<Value>,
    /// 会话标签（`SessionSwitcher` 等平台分组显示；如 `{"dm":"私聊","group":"群"}`，可选）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_labels: Option<Value>,
    /// 该插件实例贡献给模型的工具（**平台语义在此声明，宿主不认识它们**）。
    ///
    /// 每个工具名同时是 `Invoke("<adapter>:<name>")` 的 op 名；宿主负责跨实例
    /// 合并、多实例时注入 `account` 寻址参数、把调用透传回来。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolContribution>,
}

/// 插件实例能力位（缺省全 false——宿主 fail-closed，未声明的调用拒绝）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceCapabilities {
    /// 出站文本（`send_text`）。
    #[serde(default)]
    pub send_text: bool,
    /// 出站媒体（`send_media`）。
    #[serde(default)]
    pub send_media: bool,
    /// 群目录（`get_groups`）。
    #[serde(default)]
    pub groups: bool,
    /// 好友目录（`get_friends`）。
    #[serde(default)]
    pub friends: bool,
    /// 门控管理（`custom` 的 `*_gate_*` / `update_*list` / `set_owner` 等）。
    #[serde(default)]
    pub gating: bool,
    /// 登录管理（`login_status` / `login_qrcode` 等）。
    #[serde(default)]
    pub login: bool,
}
