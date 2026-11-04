//! 框架仓自守门禁 #3：**领域无关性**。
//!
//! 判据（EchoAgentCore `document/design/echo-fabric-arch-extraction.md` §2.1）：
//! 框架 crate 的生产代码不认识应用领域词汇；框架 crate 不依赖领域/内核 crate。
//!
//! 覆盖范围**暂时不含** `echo-plugin-api` / `echo-plugin-sdk`：协议的贡献分类
//! 仍是领域词汇（`Contribution::Tool` / `Skill` / `PluginInstance`，wire tag
//! 还保留着历史名 `adapter`），去业务化排在 M5 且有兼容性门槛。那两件的
//! 机制层（帧 / 握手 / serve 循环）与领域无关，只是协议表面尚未收敛——这一点
//! 在 README 与 CHANGELOG 里都已写明，不靠本门禁假装干净。
//!
//! 为什么用门禁而不是评审：`MountContext.tools` / `PluginKind::{Tool,Skill}` /
//! `PluginToolSink` 这类东西**当初都是善意加的**（"给外部插件预留的接缝"），
//! 但它们让框架认识了它不该认识的词。人读文档会漏，测试不会。

use std::path::{Path, PathBuf};

/// 参与领域词审计的框架 crate（相对本仓 `crates/`）。
const AUDITED_CRATES: &[&str] = &[
    "echo-context",
    "echo-plugin",
    "echo-plugin-loader",
    "echo-plugin-host",
];

/// 全仓禁止出现的应用领域词汇（任意 crate 的 Cargo.toml 依赖都受此约束）。
const FORBIDDEN_DEPS: &[&str] = &["echo-defs", "echo-agent", "echo-protocol", "echo-session"];

/// 框架层禁用的应用领域词汇。
///
/// 选词标准：**只有宿主应用才有意义的词**。刻意不含 `plugin` / `manifest` /
/// `service` / `event` / `transport` 这类框架自身词汇——框架当然可以有这些。
const FORBIDDEN_WORDS: &[&str] = &[
    "session",
    "timeline",
    "persona",
    "team",
    "prompt",
    "llm",
    "tool",
    "skill",
    "workspace",
    "chat",
    "agent",
];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crate dir")
        .parent()
        .expect("repo root")
        .to_path_buf()
}

/// 递归收集 `dir` 下的 `.rs` 文件（跳过 `bin/`）。
///
/// `src/bin/` 里只有 conformance 对端夹具（`echo-plugin-test-peer`），它的职责
/// 就是构造协议的各种贡献——包括协议目前仍带的领域分类（M5 待议）。它不参与
/// 对外 API 表面，因此不算"框架认识了领域词汇"。
fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "bin") {
                continue;
            }
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// 生产代码 = `#[cfg(test)]` 之前的部分（测试里可以出现任何词）。
fn production_part(text: &str) -> &str {
    match text.find("#[cfg(test)]") {
        Some(idx) => &text[..idx],
        None => text,
    }
}

/// 整词、大小写不敏感匹配：前后都不能是标识符字符。
fn contains_word(line: &str, word: &str) -> bool {
    let is_ident = |c: char| c.is_alphanumeric() || c == '_';
    // `to_ascii_lowercase` 只改 ASCII 字节，长度不变，索引可直接复用。
    let line = line.to_ascii_lowercase();
    let mut start = 0;
    while let Some(pos) = line[start..].find(word) {
        let begin = start + pos;
        let end = begin + word.len();
        let before_ok = line[..begin]
            .chars()
            .next_back()
            .is_none_or(|c| !is_ident(c));
        let after_ok = line[end..].chars().next().is_none_or(|c| !is_ident(c));
        if before_ok && after_ok {
            return true;
        }
        start = begin + 1;
        if start >= line.len() {
            break;
        }
    }
    false
}

/// 从 `Cargo.toml` 取 `[dependencies]` / `[dev-dependencies]` 里的依赖名。
fn declared_deps(manifest: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(manifest) else {
        return Vec::new();
    };
    let mut in_deps = false;
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_deps = line == "[dependencies]" || line == "[dev-dependencies]";
            continue;
        }
        if !in_deps || line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((name, _)) = line.split_once('=') {
            out.push(name.trim().to_string());
        }
    }
    out
}

#[test]
fn framework_production_code_has_no_domain_vocabulary() {
    let root = repo_root();
    let mut violations = Vec::new();

    for crate_dir in AUDITED_CRATES {
        let src = root.join("crates").join(crate_dir).join("src");
        assert!(src.is_dir(), "受审计 crate 不存在：{}", src.display());
        let mut files = Vec::new();
        rust_files(&src, &mut files);
        for file in files {
            let Ok(text) = std::fs::read_to_string(&file) else {
                continue;
            };
            for (idx, line) in production_part(&text).lines().enumerate() {
                for word in FORBIDDEN_WORDS {
                    if contains_word(line, word) {
                        violations.push(format!(
                            "{}:{}: 「{word}」— {}",
                            file.strip_prefix(&root).unwrap_or(&file).display(),
                            idx + 1,
                            line.trim()
                        ));
                    }
                }
            }
        }
    }

    assert!(
        violations.is_empty(),
        "框架层出现了应用领域词汇（框架不该认识它们；要注册领域贡献请走 \
         `Ctx::provide` 服务键，或放进宿主侧的适配层）：\n{}",
        violations.join("\n")
    );
}

/// 任意框架 crate（含受审计之外的 api/sdk）都不许依赖领域/内核 crate。
///
/// 对 api/sdk 来说"协议表面仍带领域分类"是**尚未收敛的协议问题**，与本条无关：
/// 它们同样不许 `use echo_defs::…`。
#[test]
fn no_framework_crate_depends_on_domain_or_kernel_crates() {
    let root = repo_root();
    let mut violations = Vec::new();

    let crates_dir = root.join("crates");
    for entry in std::fs::read_dir(&crates_dir)
        .into_iter()
        .flatten()
        .flatten()
    {
        let manifest = entry.path().join("Cargo.toml");
        if !manifest.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        for dep in declared_deps(&manifest) {
            if FORBIDDEN_DEPS.contains(&dep.as_str()) {
                violations.push(format!("{name} → {dep}"));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "框架 crate 依赖了领域/内核 crate（领域适配应落在宿主侧）：\n{}",
        violations.join("\n")
    );
}

/// 门禁自检：整词匹配不误报、不漏报。
#[test]
fn word_boundary_matching_is_sound() {
    // 不该命中：复数 / 复合词
    assert!(!contains_word("let tools = 1;", "tool"));
    assert!(!contains_word("// EchoAgentCore brand", "agent"));
    assert!(!contains_word("skills_dir", "skill"));

    // 该命中：整词（含 CamelCase）
    assert!(contains_word("let tool = 1;", "tool"));
    assert!(contains_word("/// a live agent", "agent"));
    assert!(contains_word("pub struct Session", "session"));
}

/// 门禁自检：`#[cfg(test)]` 之后的内容必须被忽略。
#[test]
fn test_modules_are_excluded() {
    let src = "pub fn f() {}\n#[cfg(test)]\nmod tests { fn g() { let tool = 1; } }";
    assert!(!contains_word(production_part(src), "tool"));
}
