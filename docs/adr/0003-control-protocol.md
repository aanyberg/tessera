# ADR 0003: Control socket speaks JSON-RPC 2.0 over a Unix socket

**Status:** accepted · 2026-10-27

## Context

Scripts, editors, automation and AI agents need to drive the terminal: open
tabs, split panes, send text, read output. iTerm2 (WebSocket + protobuf),
WezTerm and kitty (Unix socket) all ship this. Options for transport: Unix
socket, localhost TCP, XPC, AppleScript. For protocol: JSON-RPC, gRPC,
MessagePack-RPC, tmux-style text.

## Decision

Unix domain socket at `~/Library/Application Support/Tessera/control.sock`
(mode 0600), newline-delimited JSON-RPC 2.0. Types live in
`tessera-protocol` with `serde` and, later, `schemars` for published JSON
Schema. Methods mirror the plugin `Host` API. Child shells receive
`TESSERA_SOCKET` and `TESSERA_SESSION_ID`. Non-descendant clients trigger a
consent prompt with a remembered allow list.

An MCP (Model Context Protocol) server adapter will translate MCP tool calls
to these methods so Claude Code, Codex and Copilot can control Tessera
natively.

## Consequences

- Any language can be a client; `nc` and `jq` suffice for debugging.
- MCP is JSON-RPC underneath, so the adapter is thin.
- gRPC rejected: heavier, hostile to shell scripts, would still need a JSON
  layer for MCP. TCP rejected: needless exposure. XPC and App Intents may be
  added later as macOS-only conveniences calling the same core.
