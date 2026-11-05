#!/usr/bin/env python3
"""最小模块——用 **Python** 写，不依赖任何 Rust 代码。

这个文件的全部意义：证明**协议就是 ABI**。你不需要 Rust、不需要 `echo-module-sdk`，
只要按 4 字节小端长度前缀 + JSON 收发帧，就能当一个模块。

相对 Node 生态的模块框架，这是实质优势：**不锁定语言**。宿主是 Rust，模块可以
是 Python / Go / Node / shell——因为边界只有管道上的字节。

线上协议（与 `echo-module-api` 的 `PROTOCOL_VERSION = 1` 对应）
──────────────────────────────────────────────────────────────
帧：`[4 字节小端 u32 长度][该长度的 UTF-8 JSON]`，两个方向都一样。

握手（顺序固定）：
  宿主 → `{"type":"hello","payload":{"protocol":"echo-module","version":1,
                                     "module_id":"…","config":{…}}}`
  模块 → `{"type":"welcome","payload":{"protocol":"echo-module","version":1,
                                      "module_id":"…","capabilities":[…]}}`
  模块 → `{"type":"register","payload":{"contributions":[…]}}`
  模块 → `{"type":"ready"}`            ← 注意：unit 变体**没有** payload 键

调用：
  宿主 → `{"type":"invoke","payload":{"call_id":"…","contribution":"greeter",
                                     "ctx":{…},"payload":{…}}}`
  模块 → `{"type":"invoke_result","payload":{"call_id":"…","outcome":{…}}}`
  outcome 是外部标签：`{"status":"ok","text":"…"}` 或
                     `{"status":"error","code":"…","message":"…"}`

生命周期：
  `{"type":"drain","payload":{"deadline_ms":N}}` → 处理完在途调用后退出
  `{"type":"dispose"}`                          → 立即退出
  `{"type":"cancel","payload":{"call_id":"…"}}` → 尽力取消
  stdin EOF                                     → 退出

硬规矩：**帧只走 stdout，日志走 stderr**。往 stdout 混入一个字节就破坏协议。
"""

from __future__ import annotations

import json
import sys

PROTOCOL_NAME = "echo-module"
PROTOCOL_VERSION = 1
SERVICE_KEY = "greeter"


def read_exact(stream, count: int) -> bytes | None:
    """读满 count 字节；对端关闭返回 None。

    必须循环读：管道是流，一次 read 可能只到一部分（发大帧时尤其明显）。
    """
    chunks = []
    remaining = count
    while remaining > 0:
        chunk = stream.read(remaining)
        if not chunk:
            return None
        chunks.append(chunk)
        remaining -= len(chunk)
    return b"".join(chunks)


def read_frame(stream) -> dict | None:
    header = read_exact(stream, 4)
    if header is None:
        return None
    length = int.from_bytes(header, "little")  # 小端 u32
    body = read_exact(stream, length)
    if body is None:
        return None
    return json.loads(body.decode("utf-8"))


def write_frame(stream, message: dict) -> None:
    body = json.dumps(message, separators=(",", ":")).encode("utf-8")
    stream.write(len(body).to_bytes(4, "little") + body)
    stream.flush()  # 关键：不 flush 宿主会一直等


def log(message: str) -> None:
    print(f"[py-module] {message}", file=sys.stderr, flush=True)


def main() -> int:
    stdin = sys.stdin.buffer
    stdout = sys.stdout.buffer

    while True:
        message = read_frame(stdin)
        if message is None:
            log("stdin closed, exiting")
            return 0

        kind = message.get("type")
        payload = message.get("payload") or {}

        if kind == "hello":
            hello = payload
            if hello.get("protocol") != PROTOCOL_NAME:
                write_frame(stdout, {"type": "failed", "payload": {
                    "code": "protocol_mismatch",
                    "message": f"expected {PROTOCOL_NAME}, got {hello.get('protocol')}",
                }})
                return 1
            if hello.get("version") != PROTOCOL_VERSION:
                write_frame(stdout, {"type": "failed", "payload": {
                    "code": "version_mismatch",
                    "message": f"expected {PROTOCOL_VERSION}, got {hello.get('version')}",
                }})
                return 1
            module_id = hello.get("module_id", "")
            log(f"hello from host, module_id={module_id} config={hello.get('config')}")

            write_frame(stdout, {"type": "welcome", "payload": {
                "protocol": PROTOCOL_NAME,
                "version": PROTOCOL_VERSION,
                "module_id": module_id,  # 回显：与 Hello 一致（不一致宿主会拒绝）
                "capabilities": ["tools", "cancel"],
            }})
            write_frame(stdout, {"type": "register", "payload": {"contributions": [
                {"kind": "service", "key": SERVICE_KEY,
                 "service_kind": "greeter", "version": 1},
            ]}})
            write_frame(stdout, {"type": "ready"})

        elif kind == "invoke":
            call_id = payload.get("call_id", "")
            contribution = payload.get("contribution", "")
            args = payload.get("payload") or {}
            log(f"invoke {contribution} args={args}")

            if contribution != SERVICE_KEY:
                write_frame(stdout, {"type": "invoke_result", "payload": {
                    "call_id": call_id,
                    "outcome": {"status": "error", "code": "unknown_contribution",
                                "message": f"unknown contribution: {contribution}"},
                }})
                continue

            name = args.get("name") or "world"
            write_frame(stdout, {"type": "invoke_result", "payload": {
                "call_id": call_id,
                "outcome": {"status": "ok",
                            "text": json.dumps({"greeting": f"hello, {name}"})},
            }})

        elif kind == "drain":
            log(f"drain (deadline {payload.get('deadline_ms')}ms), all calls done, exiting")
            return 0

        elif kind == "dispose":
            log("dispose, exiting now")
            return 0

        elif kind == "cancel":
            log(f"cancel {payload.get('call_id')} (this module answers synchronously)")

        elif kind == "event":
            log(f"event {payload.get('event')}")

        else:
            log(f"unknown message type: {kind}")


if __name__ == "__main__":
    try:
        sys.exit(main())
    except BrokenPipeError:
        # 宿主先退出：正常收场，不要打 traceback 污染 stderr。
        sys.exit(0)
