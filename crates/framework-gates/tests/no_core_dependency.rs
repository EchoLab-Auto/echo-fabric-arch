//! 框架仓自守门禁 #1：**不许反向依赖**。
//!
//! 框架是依赖图的下游终点——只允许指向第三方的通用库（serde / tokio / …）。
//! 一旦某个框架 crate 依赖了宿主应用的 crate（`echo-defs` / `echo-agent` /
//! `echo-protocol` …），"业务无关"就成了口号：框架会跟着应用一起演进，
//! 第三方宿主也就无法单独使用它。
//!
//! 为什么门禁在框架仓而不是宿主仓：宿主侧的 `scripts/deps-policy.txt` 只扫
//! 自己的 `source/` 目录树，本仓的 crate 迁出后**自动不在扫描范围内**——
//! 那边的保护随迁移消失，必须由本仓补上等价断言（见 EchoAgentCore 的
//! `document/design/echo-fabric-arch-extraction.md` §4.2）。
//!
//! 白名单只有一条：`echo-plugin-sdk` → `echo-plugin-api`（同仓内上下游）。

use std::path::{Path, PathBuf};

/// 本仓内允许的 echo-* 依赖：`(crate 目录名, 允许的 echo-* 依赖)`。
const INTERNAL_EDGES: &[(&str, &[&str])] = &[
    ("echo-plugin-api", &[]),
    ("echo-plugin-sdk", &["echo-plugin-api"]),
];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crate dir")
        .parent()
        .expect("repo root")
        .to_path_buf()
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
fn framework_crates_only_depend_on_each_other_as_declared() {
    let root = repo_root();
    let mut violations = Vec::new();

    for (crate_dir, allowed) in INTERNAL_EDGES {
        let manifest = root.join("crates").join(crate_dir).join("Cargo.toml");
        assert!(
            manifest.is_file(),
            "本仓没有 {}——新增框架 crate 时请同步登记 INTERNAL_EDGES",
            manifest.display()
        );
        for dep in declared_deps(&manifest) {
            if !dep.starts_with("echo-") {
                continue;
            }
            if !allowed.contains(&dep.as_str()) {
                violations.push(format!("{crate_dir} → {dep}"));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "框架 crate 出现了未登记的 echo-* 依赖。框架只能指向通用第三方库；\
         领域 crate（echo-defs 等）属于宿主应用，不进本仓：\n{}",
        violations.join("\n")
    );
}

/// 反向自检：确认白名单机制本身有效（否则本门禁可能永远为空转）。
#[test]
fn edge_allowlist_actually_restricts() {
    let sdk_allowed = INTERNAL_EDGES
        .iter()
        .find(|(name, _)| *name == "echo-plugin-sdk")
        .map(|(_, allowed)| *allowed)
        .expect("echo-plugin-sdk 必须在 INTERNAL_EDGES 里");
    assert!(
        sdk_allowed.contains(&"echo-plugin-api"),
        "sdk 依赖 api 是合法边"
    );

    let api_allowed = INTERNAL_EDGES
        .iter()
        .find(|(name, _)| *name == "echo-plugin-api")
        .map(|(_, allowed)| *allowed)
        .expect("echo-plugin-api 必须在 INTERNAL_EDGES 里");
    assert!(
        api_allowed.is_empty(),
        "api 是契约层叶子，不允许任何 echo-* 依赖"
    );
}
