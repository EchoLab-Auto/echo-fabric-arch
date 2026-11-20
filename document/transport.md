---
id: transport
title: 运输层
group: 概念
---

# 运输层

同一个模块契约，三种载体。换载体不改模块代码——这是"协议即 ABI"的实际含义。

## 进程内（`InprocTransport`）

模块是实现 `Module` 的 Rust 对象，编译进宿主二进制。注册是函数调用，参数与
返回值**不序列化**。

- 优点：零序列化、可以直接传 trait object、调试时栈是连续的；
- 缺点：模块 panic 会带崩宿主；模块与宿主共用 Rust 版本和 crate 版本。

## 进程外（`StdioTransport`）

模块是独立二进制，通过 stdin/stdout 通信。

- 优点：崩溃隔离、语言无关、可以单独升级、能加资源限额；
- 缺点：每次调用要序列化；模块拿不到宿主的 Rust 类型。

## 动态库（`DylibTransport`）

模块编译成 `.so`，宿主 `dlopen` 后把一条 socketpair 的模块侧 fd 交给它，
**运行期插入宿主进程**。帧协议与 stdio **逐位一致**（共用
`echo-module-host::frames` 这一份编解码）。

- 优点：**独立产物 + 同进程**——这是另外两种载体各自都给不出的组合；
- 缺点：**没有崩溃隔离**（`.so` 的 segfault 直接带走宿主）、**不能卸载**
  （`dlclose` 带线程的 Rust cdylib 不安全，映射保留到进程退出）。

跨边界面只有一个 C 符号：

```c
int echo_module_run(int fd);
```

没有 Rust 类型过 FFI，所以不需要 ABI 稳定层，也不需要每个模块手写
`extern "C"` 桥。模块侧用 `echo_module_sdk::serve_fd_blocking` 一行接上：

```rust
#[no_mangle]
pub extern "C" fn echo_module_run(fd: c_int) -> c_int {
    serve_fd_blocking(|| MyModule, fd)
}
```

**panic 必须接在 `.so` 内部**：`extern "C"` 是 nounwind，Rust 会把穿出的 panic
改成 `abort`——带走的不是模块，是整个宿主进程。`serve_fd_blocking` 已经把
整个流程（含建 runtime、接 fd）包在 `catch_unwind` 里。`segfault` / `abort` /
OOM 仍接不住，这是形态本身的代价，不是实现缺陷。

关停语义与其它载体不同：**没有可杀的对象**。宿主只能关掉写方向（模块读到
EOF 自行退出）然后等；等不到就以 `Timeout` 收场并**泄漏该线程**。所以
`dylib` 适合"自己写的、稳定的"模块；崩溃风险高的（第三方代码、平台适配器）
应当用 stdio。

## 帧格式

```
[4 字节小端长度][JSON 载荷]
```

长度前缀而不是换行分帧——JSON 里可以合法出现换行，按行切会碎。

单帧上限 16 MiB。**日志必须写 stderr**：stdout 是协议通道，混进任何字节都会
破坏帧（这是新模块最常见的第一个 bug）。

## 取消安全（`recv` / `send`）

`recv` / `send` 的 future 可能被 `select!` 或调用方取消。运输层实现把**半帧
进度保存在连接内**（stdio：连接级读缓冲 / 写偏移），因此：

- 在帧中途取消 `recv` 不丢已读字节，下一次从断点续读，流不会错位；
- 在帧中途取消 `send` 时半帧留在连接内，下一次 `send` 先补完它再写新帧；
  因此取消**不等于**「一定没发送」。

**半帧与停止**：连接被关停时若写方向留着半帧（`send` 被取消 / 超时 / 写失败），
实现**不**清空缓冲假装没发过——那会让对端读到被截断的帧。此时的策略是
**强杀（`start_kill`）并封死写方向**（之后所有 `send` 直接 `Closed`），保证
对端不可能把后续字节当成这半帧的续体而误执行。写方向一旦写坏（管道破裂 /
只写出一半），同样封死，绝不把新帧追加到残帧后面。

## 写阻塞与「有界」

`send` 会**阻塞**：对端不读 stdin 时管道几十 KiB 就满，`write` 一直等可写。
因此凡是「承诺有界」的地方都把它算进预算里：

| 操作 | 预算 |
|---|---|
| `invoke` | `ctx.deadline_ms`（或默认 600s）**整体**覆盖「发送 + 等结果」 |
| `drain` / `stop` | 同一个 `deadline` 覆盖 `Drain` 的发送 + 等模块退出 + `shutdown` |
| `shutdown` | 取写锁最多 `min(deadline, 50ms)`；拿不到 / 有半帧 → 直接强杀（有界收尸） |

## 握手顺序

```prodoc-flow
graph LR
  H1[宿主: Hello<br/>version + config] --> P1[模块: Welcome<br/>id + version + capabilities]
  P1 --> P2[模块: Register<br/>贡献清单]
  P2 --> Ready[就绪：可 Invoke]
  Ready --> Inv[Invoke / Cancel / HostCall]
  Ready --> Drain[Drain → Dispose]
```

两步校验都是 **fail-closed**：

- 协议名或版本不兼容 → 直接拒绝，不做"先连上再试试"；
- `Register` 里声明的贡献之外的东西，宿主调用时返回结构化错误，不静默放行。

`HostCall`（模块回呼宿主服务）是这条通道上的反方向消息，单独成篇：
[宿主服务与回呼](./host-service.md)。

## 进程外轨的治理项

| 项 | 缺省 | 说明 |
|---|---|---|
| 崩溃重启 | on-failure | backoff 200ms 起、×2、上限 2s、5 次后熔断 |
| 资源限额 | **不限制** | `rlimit`：CPU / 地址空间 / 单文件 / fd / 进程数。0 会被拒绝（`setrlimit` 里 0 = 完全禁止） |
| 速率限额 | Emit 2000/s、HostCall 500/s、Log 200/s | 只限"投递即忘"的帧；`InvokeResult` **绝不丢** |
| 环境 | `env_clear` + 仅 `PATH` + 显式 env | 不继承宿主的全部环境变量 |
| 排空 | 卸载先 `Drain`，超时再杀 | 在途调用以 `drained` 收尾（热替换旧实例为 `replaced`） |

## 什么时候该用哪个

| 场景 | 建议 |
|---|---|
| 模块由同一团队维护、与宿主同步发布 | 进程内 |
| 第三方模块 / 语言不同 / 可能失控 | 进程外（stdio） |
| 每个 token 都要走的热路径 | 进程内 |
| 需要在运行期单独升级模块 | 进程外（`upgrade` 原子切流量） |
| 想要**独立产物**（单独构建 / 单独发布）**又不想多一个进程** | `dylib` |
| 模块数多（每个进程一份 runtime 与常驻内存），且都信得过 | `dylib` |

`dylib` 的取舍只有一条横线：**它放弃了崩溃隔离**。所以判断标准不是"性能"
（三种载体都在每轮一次的量级上，socketpair 的 syscall 相对 LLM 调用可忽略），
而是"这个模块崩了能不能接受宿主一起崩"。

`wasm` 仍是可选轨（**当前未实现**，`EntryKind` 里留了位置）：装载器能解析该
条目，但框架没有对应运输层——宿主拿到它只能明确拒绝，不能假装它能起来。
