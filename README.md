# Tessera

A macOS terminal emulator with iTerm2's product sensibility on a modular Rust core.

> **Status:** pre-alpha. The workspace compiles, the architecture is fixed, the
> emulator is not yet usable. See [Milestones](#milestones).

## Why

- **iTerm2** is the most intuitive terminal on macOS, but it is a large
  Objective-C codebase that is slow in places and hard to extend.
- **WezTerm** has an excellent Rust core and deep configurability, but a
  non-native UI and Lua-only configuration that most users never touch.

Tessera keeps what makes iTerm2 approachable (native windows and tabs, a real
Preferences window, profiles, shell integration) and builds it on a Rust core
that is designed from the first commit for plugins, external control and
future frontends on other platforms.

## Architecture in one picture

```
Tessera.app (Swift, thin)   windows · tabs · menus · Preferences · input
        │  C ABI (tessera-ffi)
tessera-app-core            workspace model · commands · config · plugin host · control
  pty · vt · grid · term · render (wgpu/Metal) · search · shell-integration
  config · plugin-api · protocol · control
```

Rules: dependencies point downward only; everything below the Swift shell is
OS-agnostic; plugins and the control socket share one host API; everything that
crosses a boundary is `serde`-serializable. Details in
[`docs/architecture.md`](docs/architecture.md) and the ADRs in
[`docs/adr/`](docs/adr/).

## Building

Requirements: macOS 15+, Xcode 16+, Rust stable (via `rustup`), and
`brew install just cbindgen cargo-deny cargo-nextest xcodegen`.

```sh
just check   # fmt, clippy -D warnings, tests, cargo-deny
just app     # universal xcframework + Xcode build
just run     # launch Tessera.app
```

Rust targets for the universal build:

```sh
rustup target add aarch64-apple-darwin x86_64-apple-darwin
```

## Development with agents

Milestone work runs as an agent team over a fixed roster of project agents in `.claude/agents/`; see [`docs/software-factory.md`](docs/software-factory.md).

## Roadmap

Work is tracked in [GitHub milestones](https://github.com/aanyberg/tessera/milestones).

| Milestone | Scope |
|---|---|
| [0.1.0](https://github.com/aanyberg/tessera/milestone/1) | Daily-drivable basics: headless emulator passing vttest basics, wgpu renderer, native window with tabs, one profile, `config.toml` from disk |
| [0.2.0](https://github.com/aanyberg/tessera/milestone/2) | Product surface: splits, profiles, Preferences window, themes, transparency/blur, shell integration marks, search, built-in plugin host, signed builds |
| [Backlog](https://github.com/aanyberg/tessera/milestone/3) | Control socket and CLI, MCP adapter, agent-usage plugin, WASM plugins, images, tmux, Linux frontend |

A 1.0.0 milestone will be defined once the 0.x releases are stable.

## Plugins

Plugins implement one trait (`tessera-plugin-api`) and declare the
capabilities they need. v1 links first-party plugins into the binary and
toggles them from Preferences. v2 loads third-party plugins as sandboxed WASM
components using the same API. The first real first-party plugin will be an
agent-usage tracker for Claude Code, Codex and Copilot.

## License

MIT OR Apache-2.0, at your option.
