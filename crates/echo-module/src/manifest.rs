//! Module manifest — the declarative identity of a module.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// 模块的**运行形态**：实现跑在本进程内，还是由独立子进程提供。
///
/// 与 [`ModuleManifest::builtin`] **正交**——这是两个不同的问题：
///
/// | 字段 | 回答 |
/// | --- | --- |
/// | `builtin` | 这条 manifest 从哪来（编译内置 / 模块目录发现） |
/// | `runtime` | 模块的实现代码在哪跑（本进程 / 独立子进程） |
///
/// 实例模块化（P1.9，2026-10）之后二者不再同义：QQ 模块实例的**门控
/// manifest** 编译在 Core 里（`builtin = true`），但模块实例实现由独立进程
/// `echo-instance-qq` 提供（见 `modules_runtime`）。此前 Panel 只能看到
/// `builtin`，于是把「跑在独立进程」的模块显示成「内部模块」——用户看到的
/// 描述与分组互相矛盾（2026-10-18 报障）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModuleRuntime {
    /// 实现编译在 Core 进程内：挂载/卸载即生效。
    #[default]
    InProcess,
    /// 实现在独立子进程（stdio 模块协议）：Core 侧只保留启停门控。
    Subprocess,
}

impl ModuleRuntime {
    /// Wire spelling (`in_process` / `subprocess`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InProcess => "in_process",
            Self::Subprocess => "subprocess",
        }
    }

    /// 是否由独立子进程提供实现。
    pub fn is_subprocess(self) -> bool {
        matches!(self, Self::Subprocess)
    }

    /// serde `skip_serializing_if` 用（签名需 `&self`）：进程内不外发该字段。
    fn is_in_process(&self) -> bool {
        matches!(self, Self::InProcess)
    }
}

/// Declarative manifest for a module.
///
/// `id` is the stable key (e.g. `acme.things.widget`); `entry` names the mount
/// entry point within the host's builtin registry (empty for external
/// manifests that carry their own payload).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModuleManifest {
    /// Stable unique id: `{vendor}.{group}.{name}`.
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub description: String,
    /// 模块类别——**开放标签**（宿主自定义），不是框架枚举。
    ///
    /// 框架只把它透传（manifest → 注册表 → 前端快照），**从不按它分支决策**：
    /// 2026-10 M2 审计发现框架内唯一用法是转成 `String` 填
    /// [`ModuleDescriptor::kind`]，其余 `match` 全是 `io::ErrorKind` /
    /// `EntryKind`（运输形态）这类同名不同物的东西。
    ///
    /// 历史：这里曾是 8 个变体的领域枚举（宿主应用的分类词汇），被误写进
    /// 框架契约——第三方宿主用 `kind = "widget"` 会当场解析失败。改为字符串
    /// 后词汇表归宿主，框架只透传。来龙去脉见
    /// `document/design/echo-fabric-arch-extraction.md` §M2。
    ///
    /// [`ModuleDescriptor::kind`]: crate::registry::ModuleDescriptor::kind
    pub kind: String,
    /// Builtin mount entry name (composition root resolves it); empty for
    /// data modules discovered from a module directory.
    #[serde(default)]
    pub entry: String,
    /// Human-facing metadata shown in the Panel.
    #[serde(default)]
    pub author: String,
    /// 是否为 Core 编译内置模块；外部 manifest 缺省为 false。
    ///
    /// 注意：这是**来源**维度，不是运行位置——进程外模块同样可以是
    /// 编译内置的门控 manifest（见 [`ModuleRuntime`]）。
    #[serde(default)]
    pub builtin: bool,
    /// 运行形态：实现跑在本进程内还是独立子进程（缺省 = 进程内）。
    #[serde(default, skip_serializing_if = "ModuleRuntime::is_in_process")]
    pub runtime: ModuleRuntime,
    /// Owning **package** id — the cross-cutting tag spanning
    /// module + tools + skills（见 [`Self::package_id`]）。
    /// `None` = 本模块自身即包根：模块 id 同时是包标签（当前全部内置
    /// 模块如此，因此默认行为与历史一致）。声明后，本模块的启停门控
    /// 应以包 id 传播到同名包的工具与技能。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
    /// 硬依赖的**服务键**：挂载前必须已在 `Ctx` 服务表里，否则拒绝挂载。
    ///
    /// 与 [`Module::inject`](crate::Module::inject) 同义，但**声明在清单里**
    /// 而不是散在构造代码中：清单是模块身份的单一真值来源，依赖关系属于身份
    /// 的一部分。散在 `with_inject(...)` 里的后果是可读性为零——想知道"这个
    /// 模块要什么"必须读代码并跟到每个键的定义处。
    ///
    /// 键是**服务键**（`Ctx` 的字符串键），不是模块 id：一个键可能由宿主
    /// 注册（`echo-agent.host.*`）也可能由另一个模块 `provide`。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires: Vec<String>,
    /// 软依赖：只声明，缺失不阻止挂载（面板据此画"弱依赖"）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub optional_requires: Vec<String>,
    /// 本模块 `provide` 到服务表的键。
    ///
    /// 此前这一半信息**完全不存在**：`Module` trait 只有 `inject`，服务靠
    /// mount 闭包里命令式 `ctx.provide(...)` 注册，于是"谁提供了这个键"
    /// 只能靠人肉搜索 `provide(` 调用。面板的依赖图因此只能硬编码一张表。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provides: Vec<String>,
}

impl ModuleManifest {
    /// 本模块所属的**包** id：显式声明优先，未声明时回退为模块 id。
    ///
    /// Package 是**跨贡献类型的标签**：本 manifest 声明归属，宿主把同一标签
    /// 打到它自己的各类贡献上（工具 / 技能 / 实例……由宿主决定有什么），
    /// 包级门控（禁用一个包 = 其全部成员不可见）按这个字符串聚合。
    ///
    /// 框架只负责把标签带到前端快照里，不解释标签的含义。
    pub fn package_id(&self) -> &str {
        self.package.as_deref().unwrap_or(&self.id)
    }

    /// 显式声明所属包（builder 风格；未调用时包 id = 模块 id）。
    pub fn with_package(mut self, package: impl Into<String>) -> Self {
        self.package = Some(package.into());
        self
    }

    pub fn builtin(
        id: impl Into<String>,
        name: impl Into<String>,
        version: impl Into<String>,
        kind: impl Into<String>,
        entry: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            version: version.into(),
            description: description.into(),
            kind: kind.into(),
            entry: entry.into(),
            package: None,
            author: "EchoAgentCore".into(),
            builtin: true,
            runtime: ModuleRuntime::InProcess,
            requires: Vec::new(),
            optional_requires: Vec::new(),
            provides: Vec::new(),
        }
    }

    /// 从 `module.toml` 的文本构造清单（`version` 由宿主注入）。
    ///
    /// 版本号不走 TOML：它是**宿主**的版本（同一份源码跟着 Core 一起发版），
    /// 写在模块文件里就得每次发版都改十几个文件，且改漏了没人发现。
    ///
    /// 解析失败返回 [`ModuleError::Parse`]（带行列信息）。调用方通常是
    /// `include_str!` + `expect`：内嵌资源解析不了必然是构建期 bug，早失败
    /// 比带着半个清单跑起来好。
    pub fn from_toml(text: &str, version: impl Into<String>) -> Result<Self, ModuleError> {
        let mut manifest = parse_manifest(text)?;
        manifest.version = version.into();
        Ok(manifest)
    }

    /// 声明运行形态（builder 风格；未调用 = 进程内）。
    ///
    /// 进程外模块（实例模块等）注册门控 manifest 时**必须**调用：否则
    /// Panel 会把「独立进程」的模块显示成「内部模块」（2026-10-18 报障）。
    /// 描述里写「独立进程」不算数——前端读的是这个字段。
    pub fn with_runtime(mut self, runtime: ModuleRuntime) -> Self {
        self.runtime = runtime;
        self
    }

    /// 声明**来源**：`false` = 不是编译进本二进制的模块（`modules.toml` 的
    /// 外部条目、模块目录发现的 `module.toml`）。
    ///
    /// 与 [`Self::with_runtime`] 正交：外部条目**也可能是进程内**实现
    /// （数据模块），而编译内置的门控壳**也可能是进程外**实现（实例模块）。
    /// 面板的来源分组读的是 `runtime`，来源徽标读的是这个字段——两者不能
    /// 互相顶替（2026-10 调研发现的归类错误即源于此）。
    pub fn with_builtin_flag(mut self, builtin: bool) -> Self {
        self.builtin = builtin;
        self
    }
}

/// Errors from manifest parsing/validation.
#[derive(Debug, Error)]
pub enum ModuleError {
    #[error("module manifest parse error: {0}")]
    Parse(String),
    #[error("module manifest missing required field: {0}")]
    MissingField(String),
    #[error("module id conflict: {0}")]
    IdConflict(String),
}

/// Parse a `module.toml` manifest from text.
pub fn parse_manifest(text: &str) -> Result<ModuleManifest, ModuleError> {
    let manifest: ModuleManifest =
        toml::from_str(text).map_err(|e| ModuleError::Parse(e.to_string()))?;
    validate_manifest(&manifest)?;
    Ok(manifest)
}

/// Parse a manifest file on disk.
pub fn load_manifest(path: &std::path::Path) -> Result<ModuleManifest, ModuleError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| ModuleError::Parse(format!("{}: {e}", path.display())))?;
    parse_manifest(&text)
}

fn validate_manifest(manifest: &ModuleManifest) -> Result<(), ModuleError> {
    if manifest.id.trim().is_empty() {
        return Err(ModuleError::MissingField("id".into()));
    }
    if manifest.name.trim().is_empty() {
        return Err(ModuleError::MissingField("name".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_manifest_toml() {
        let text = r#"
id = "echo-agent.tools.builtin"
name = "内置工具集"
version = "0.1.0"
description = "平台无关的内置工具"
kind = "tool"
entry = "builtin_tools"
author = "EchoAgentCore"
"#;
        let m = parse_manifest(text).unwrap();
        assert_eq!(m.id, "echo-agent.tools.builtin");
        assert_eq!(m.kind, "tool");
        assert_eq!(m.entry, "builtin_tools");
    }

    #[test]
    fn missing_id_is_error() {
        // 覆盖**不能因新增 builtin 而丢**：旧版就用这条守住"缺 id 必须报错"。
        let text = r#"
name = "x"
kind = "skill"
"#;
        // Missing id either fails deserialization (kind present but id absent
        // → missing required field) or validation; both are errors.
        assert!(parse_manifest(text).is_err());
    }

    #[test]
    fn blank_id_is_error() {
        // 显式空 id 走 validate_manifest 分支（与上面 serde 分支互补）。
        let text = r#"
id = "   "
name = "x"
kind = "skill"
"#;
        assert!(matches!(
            parse_manifest(text),
            Err(ModuleError::MissingField(field)) if field == "id"
        ));
    }

    #[test]
    fn external_manifest_defaults_to_non_builtin() {
        let text = r#"
 id = "example.external"
 name = "外部模块"
 kind = "tool"
 "#;
        let m = parse_manifest(text).unwrap();
        assert!(!m.builtin);
    }

    /// 2026-10-18 报障：QQ 模块实例描述里写「独立进程」，Panel 却按
    /// `builtin=true` 归到「内部模块」——两个维度被挤进一个字段。
    /// 现在 `runtime` 独立表达运行形态。
    #[test]
    fn runtime_is_orthogonal_to_builtin() {
        // 默认 = 进程内，且外发时省略该字段（旧前端零变化）
        let plain = ModuleManifest::builtin(
            "echo-agent.tools.builtin",
            "内置工具集",
            "0.1.0",
            "tool",
            "builtin",
            "desc",
        );
        assert_eq!(plain.runtime, ModuleRuntime::InProcess);
        assert!(!plain.runtime.is_subprocess());
        let json = serde_json::to_string(&plain).unwrap();
        assert!(!json.contains("runtime"), "进程内不外发: {json}");

        // 实例模块：builtin 仍为 true（manifest 编译内置），但实现跑在
        // 独立进程——两者互不干扰。
        let instance = ModuleManifest::builtin(
            "echo-agent.instance.qq",
            "QQ 模块实例",
            "0.1.0",
            "instance",
            "qq",
            "OneBot v11（独立进程）",
        )
        .with_runtime(ModuleRuntime::Subprocess);
        assert!(instance.builtin, "来源维度不受 runtime 影响");
        assert!(instance.runtime.is_subprocess());
        assert_eq!(instance.runtime.as_str(), "subprocess");
        let json = serde_json::to_string(&instance).unwrap();
        assert!(json.contains("\"runtime\":\"subprocess\""), "{json}");
        let back: ModuleManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.runtime, ModuleRuntime::Subprocess);
    }

    /// 旧 manifest（无 `runtime` 键）反序列化 = 进程内（向后兼容）。
    #[test]
    fn legacy_manifest_without_runtime_deserializes_as_in_process() {
        let text = r#"
 id = "example.external"
 name = "外部模块"
 kind = "tool"
 "#;
        let m = parse_manifest(text).unwrap();
        assert_eq!(m.runtime, ModuleRuntime::InProcess);
    }

    #[test]
    fn open_kind_vocabulary_roundtrips() {
        // 框架不认识词汇表：任意标签都能解析、透传、往返（M2 去业务化）。
        // 历史：`kind` 曾是 8 变体枚举，`kind = "widget"` 当场解析失败——
        // 第三方宿主想用自己的分类就得先改框架源码。
        let text = r#"
 id = "acme.widget.gizmo"
 name = "第三方模块"
 kind = "widget"
 "#;
        let m = parse_manifest(text).expect("未知 kind 不应导致解析失败");
        assert_eq!(m.kind, "widget");

        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains("\"kind\":\"widget\""), "{json}");
        let back: ModuleManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.kind, "widget");
    }

    #[test]
    fn roundtrip_serialization() {
        let m = ModuleManifest::builtin("a.b.c", "x", "1.0.0", "skill", "entry", "desc");
        let json = serde_json::to_string(&m).unwrap();
        let back: ModuleManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(m, back);
    }
}
