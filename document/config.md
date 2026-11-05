---
id: config
title: 配置组合
group: 概念
---

# 配置组合

`echo-module-loader` 解决一件事：**一份模块清单由多个文件拼出来，且每个条目
都能回答"我是被哪个文件改成了现在这样"**。

## 四层合成

```
profile（modules.toml 本体）
  ↓ 依次应用
bundles（[profile].bundles 列出的文件）
  ↓ 依次应用
patches（[profile].patches 列出的文件）
  ↓ 最后应用
inline（profile 文件自己的 [[module]] 行）
```

**语义是"按 id 定位、整行替换"**（dsh 风格），不是字段级 merge。理由很实际：
字段级 merge 会让"删除一个配置项"没有表达方式——你只能把它设成空值，而下一次
默认值变化时它不会跟着变。整行替换则语义封闭：最终那行长什么样，就是最后一个
写它的文件写的样子。

新 id 追加，已有 id 整体替换。

## 一个条目长什么样

```toml
[[module]]
id    = "greeter"
kind  = "stdio"        # builtin | stdio | dylib | wasm
name  = "greeter"
command = "/usr/local/libexec/greeter"   # stdio 必填
args  = ["--verbose"]
enabled = true
requires = ["logger"]  # 依赖排序：这些 id 必须先就绪
[module.config]        # 原样透传给模块（Hello.config / MountContext.config）
lang = "zh"
[module.limits]        # 只对 stdio 生效
memory_mb = 512
```

合成后每个条目带 `layer`（0 起的应用序号）与 `layer_source`（文件名）——
这就是 provenance，面板/CLI 靠它回答"我要去哪一层改这一行"。

## 合成期就校验的事

- `stdio` 条目必须有 `command`；id 非空且不含空白；
- `requires` 必须能解析（否则报"未知依赖 id"）；
- 按 `requires` 做**稳定拓扑排序**：有依赖关系的按依赖排，没有的保持原顺序
  （行序是配置表达的一部分——bundle 里靠前的先挂）；
- 环 → 报可读的链条（`a -> b -> c -> a`），不是"拓扑排序失败"。

## 两种用法

**启动路径**：`load(path)` 合成 → 宿主按结果装载。校验失败就是启动失败。

**排查路径**：`dump(path)` 把合成结果连同来源注释渲染回 TOML：

```toml
# --- greeter (layer 1: bundle-extra.toml) ---
id = "greeter"
kind = "stdio"
...
```

它和启动路径**共用同一个 `load()`**，所以"dump 看到的树"就是"运行期装载的树"，
不存在两套解析各说一套。

## 常见坑

| 坑 | 原因 |
|---|---|
| 改了 bundle 没生效 | 该 id 被后面的 patch 整行替换了——看 provenance |
| `requires` 循环 | 依赖不能成环（哪怕逻辑上"互补"） |
| 配置写了但不生效 | 条目 `enabled = false`，或者宿主没有对应 `EntryKind` 的运输层 |
| 凭证写进 `[module.config]` | 配置会进清单接口与日志排查面——凭证请走环境变量或宿主自己的存储 |
