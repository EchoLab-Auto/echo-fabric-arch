//! 贡献类型：插件向宿主注册的能力。
//!
//! # 只有与领域无关的形状
//!
//! 这里定义的是**协议表面**——插件与宿主之间唯一的共享词汇。所以每个变体都
//! 必须回答一个问题：**换一个应用还成立吗？**
//!
//! | 变体 | 形状 | 通用 |
//! |---|---|---|
//! | [`Service`](Contribution::Service) | 键 + 版本 | ✅ 服务注册 |
//! | [`Events`](Contribution::Events) | 事件名清单 | ✅ 订阅 |
//! | [`Command`](Contribution::Command) | 名字 + 说明 | ✅ "这类调用归我处理" |
//! | [`Text`](Contribution::Text) | 键 + 标题 + 顺序 | ✅ 有序文本块 |
//! | [`Custom`](Contribution::Custom) | 宿主自定义分类 + 载荷 | ✅ **逃生口** |
//!
//! 曾经还有三个变体是**领域分类**（面向模型的工具声明、技能目录、聊天平台插件
//! 实例与能力位）——从 EchoAgentCore 带过来的。2026-11 审计确认它们**零真实
//! 使用者**（连 EchoAgentCore 自己都改用本仓之外的副本），已删除，由
//! [`Custom`](Contribution::Custom) 承接：宿主自带分类词汇，框架只搬 `kind`
//! 字符串与 `payload`，不认识其中任何含义。
//!
//! 这条边界的收益是可验证的：删掉它们之后，框架仓的领域词门禁可以对**全部**
//! crate 生效（见 `framework-gates/tests/domain_agnostic.rs`），而不是把协议 crate
//! 长期排除在外、任其继续长业务类型。

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 一次注册的贡献。`Register` 消息携带全量集合（整体替换语义）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Contribution {
    /// 以稳定键注册的服务（供 `Ctx` 服务定位消费）。
    Service(ServiceContribution),
    /// 事件订阅：插件希望收到的宿主事件名列表。
    Events(EventContribution),
    /// 命令处理：插件声明"这类调用归我处理"。
    Command(CommandContribution),
    /// 有序文本块：插件向宿主的"文本装配"过程贡献一段文本。
    Text(TextBlockContribution),
    /// **宿主自定义的贡献**——领域词汇的唯一入口。
    ///
    /// 领域宿主（把插件当"工具提供者""消息通道""数据源"用的那种）应该用这个
    /// 变体自带分类，而不是让框架为它新增一个 enum 变体。框架只搬运，不解释：
    ///
    /// - `kind` 是宿主词汇（`"report_generator"` / `"chat_adapter"` / 任何字符串）；
    /// - `payload` 的形状由宿主与插件自行约定，框架不做 schema 校验；
    /// - `name` 参与 `Invoke` 寻址（空 = 不参与，宿主自己从 payload 里取）。
    ///
    /// 与 `ServiceContribution::kind` 同样的取舍：载荷字段不能叫 `kind`——枚举的
    /// 标签字段已经占了那个名字（`{"kind":"custom", …}`），重复键会让整帧解析
    /// 失败。所以线上名是 `custom_kind`。
    Custom {
        /// 宿主自定义的分类（框架不解释）。
        #[serde(rename = "custom_kind")]
        kind: String,
        /// `Invoke` 的寻址名；空 = 不参与寻址。
        #[serde(default, skip_serializing_if = "String::is_empty")]
        name: String,
        /// 宿主与插件约定的载荷（框架不解释、不校验）。
        #[serde(default)]
        payload: Value,
    },
}

impl Contribution {
    /// `Invoke` 的寻址目标；`None` = 这条贡献不是"被调用的"。
    ///
    /// 哪些贡献参与寻址是**协议的知识**——放在变体定义旁边，宿主与 SDK 都不必
    /// 各自 match 一遍具体变体（那会让它们也被迫认识自己不关心的分类）。
    pub fn target_name(&self) -> Option<&str> {
        match self {
            Contribution::Service(service) => Some(&service.key),
            Contribution::Command(command) => Some(&command.name),
            // 文本块是"宿主来取"而不是"宿主来调"（拉模式），不参与寻址。
            Contribution::Text(_) => None,
            Contribution::Events(_) => None,
            Contribution::Custom { name, .. } => {
                if name.is_empty() {
                    None
                } else {
                    Some(name)
                }
            }
        }
    }

    /// 宿主自定义分类（仅 [`Custom`](Contribution::Custom) 有；其它返回 `None`）。
    ///
    /// 宿主用它把自定义贡献分派给自己的注册表，不需要 match 其它变体。
    pub fn custom_kind(&self) -> Option<&str> {
        match self {
            Contribution::Custom { kind, .. } => Some(kind),
            _ => None,
        }
    }
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
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
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
