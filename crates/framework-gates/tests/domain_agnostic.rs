//! 框架仓自守门禁 #3：**领域无关性**。
//!
//! 判据（EchoAgentCore `document/design/echo-fabric-arch-extraction.md` §2.1）：
//! 框架 crate 的生产代码不认识应用领域词汇；框架 crate 不依赖领域/内核 crate。
//!
//! ## 覆盖范围与已知残留
//!
//! **全部**框架 crate 都参与审计——包括曾经被排除的 `echo-plugin-api` /
//! `echo-plugin-sdk`。被排除过的那段时间里，`echo-plugin-api` 长出了一整套
//! QQ 聊天平台类型（`MessageTarget` / `ChannelType` / `IncomingMessage`），
//! 而排查发现它们**零使用者**：EchoAgentCore 用自己的 `echo-defs::chat` 副本，
//! 本仓内部只有一条测试在测它们的 serde 往返。已删除。
//!
//! 现存残留用 [`KNOWN_DOMAIN_RESIDUE`] 显式列出（**当前为空**），而不是把整个
//! crate 排除：
//!
//! - 排除整个 crate = 这个 crate 里**新增**任何领域词都不会被发现；
//! - 白名单 = 已知的有记录、新增的会红，且每条都写着为什么还在。
//!
//! 白名单清空靠的是同批的另两步：三个领域贡献变体换成通用的
//! `Contribution::Custom { kind, payload }`（宿主自带分类），以及把历史说明里
//! 的英文词汇改写成中文表述——**门禁只认代码里的标识符，不区分解释与 API**，
//! 所以历史说明也不该把那些词写回去。
//!
//! 残留判断依据（2026-11 实测）：那三个领域分类在 EchoAgentCore 与本仓内部都只
//! 被**测试与 conformance 夹具**使用，没有真实生产者或消费者。它们最终被换成
//! 通用的 `Contribution::Custom`，而不是"留到以后再说"——因为那个"以后"不会
//! 自己到来。
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
    "echo-plugin-api",
    "echo-plugin-sdk",
];

/// 已知的领域残留：`(crate, 文件相对 crate 的路径, 词)`。
///
/// 每一条都要能回答「为什么还在」。想新增一条 = 先证明它删不掉。
///
/// **粒度是「文件 + 词」而不是「文件 + 词 + 行」**：行号会随编辑漂移，比白名单
/// 腐烂更难维护。代价是同一个文件里**新加**的 `tool` 也会被豁免——所以白名单
/// 只覆盖变体定义所在的那一个文件；新代码一律不该出现在这里。
///
/// 配套的 `known_residue_entries_must_still_be_needed` 会在某条不再触发违规时
/// 报错，逼着删掉过时条目——否则白名单只会越积越厚。
const KNOWN_DOMAIN_RESIDUE: &[(&str, &str, &str)] = &[
    // 空表：全部框架 crate（含 echo-plugin-api / echo-plugin-sdk）的生产代码
    // 已不含任何领域词。协议的自定义分类走 `Contribution::Custom`，词汇由宿主
    // 自己定义——框架一个都不认识。
    //
    // 这张表该一直是空的。真要加一条，先在 PR 里证明"删不掉"。
];

/// 一条违规是否已在白名单里。
fn is_known_residue(crate_name: &str, rel_path: &str, word: &str) -> bool {
    KNOWN_DOMAIN_RESIDUE
        .iter()
        .any(|(c, p, w)| *c == crate_name && *p == rel_path && *w == word)
}

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
            // `tests.rs` 是 `#[cfg(test)] mod tests;` 的目标文件（编译期只在
            // 测试配置下存在），与文件内的 `#[cfg(test)] mod …` 等价。把它当
            // 生产代码审，只会逼测试里换词——降低测试可读性，不会让框架更干净。
            if path.file_name().is_some_and(|n| n == "tests.rs") {
                continue;
            }
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

/// 白名单不许腐烂：每一条都必须在当前代码里**仍然触发违规**。
///
/// 没有这条，白名单只会单向变厚——某个残留删掉后条目留着，下次同样的词再出现
/// 就静默被豁免，而没人会回头检查。这条测试让"删干净了"必须同步删条目。
#[test]
fn known_residue_entries_must_still_be_needed() {
    let root = repo_root();
    let mut stale = Vec::new();

    for (crate_name, rel_path, word) in KNOWN_DOMAIN_RESIDUE {
        assert!(
            FORBIDDEN_WORDS.contains(word),
            "白名单条目「{crate_name}/{rel_path}: {word}」里的词不在 FORBIDDEN_WORDS              中——它对审计毫无作用，删掉它"
        );
        let file = root.join("crates").join(crate_name).join(rel_path);
        let Ok(text) = std::fs::read_to_string(&file) else {
            stale.push(format!("{crate_name}/{rel_path}: 文件不存在"));
            continue;
        };
        let still_there = production_part(&text)
            .lines()
            .any(|line| contains_word(line, word));
        if !still_there {
            stale.push(format!(
                "{crate_name}/{rel_path}: 已不再出现「{word}」"
            ));
        }
    }

    assert!(
        stale.is_empty(),
        "KNOWN_DOMAIN_RESIDUE 有过时条目（残留已删干净，条目却还在）。\n\
         过时条目会让下一次同样的词静默通过审计，请删掉：\n{}",
        stale.join("\n")
    );
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
            // 相对 crate 根的路径（白名单按 `(crate, 相对路径, 词)` 匹配）。
            let rel = file
                .strip_prefix(root.join("crates").join(crate_dir))
                .unwrap_or(&file)
                .display()
                .to_string()
                .replace('\\', "/");
            for (idx, line) in production_part(&text).lines().enumerate() {
                for word in FORBIDDEN_WORDS {
                    if !contains_word(line, word) || is_known_residue(crate_dir, &rel, word) {
                        continue;
                    }
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

    assert!(
        violations.is_empty(),
        "框架层出现了应用领域词汇（框架不该认识它们；要注册领域贡献请走 \
         `Ctx::provide` 服务键，或放进宿主侧的适配层）。\n\
         若确实删不掉（线上协议已发布等），把它连同理据加进 KNOWN_DOMAIN_RESIDUE：\n{}",
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
