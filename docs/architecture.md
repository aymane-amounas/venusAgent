# Architecture, current status, and implementation plan

This is a record of what the repository actually implements today, not where I want it to end up. When something is partial or missing, it says so.

## Current architecture

```text
User
 |
 v
Rust CLI and terminal UI (src/main.rs, src/tui.rs)
 |-- config.rs ------> per-user TOML configuration
 |-- repository.rs --> ignore-aware workspace inventory
 |                     fixed Git status invocation
 |-- provider.rs ----> OpenAI-compatible / Ollama / Anthropic / Gemini HTTP adapters
 |-- capability.rs --> provider-independent contracts and in-memory registry
 |-- orchestrator.rs -> bounded, permission-aware capability execution
 |-- permissions.rs -> profiles, grants, approval evaluation, audit types
 `-- database.rs ----> local SQLite schema v3
                         |-- workspaces
                         |-- descriptive tasks
                         |-- model interaction metadata
                         `-- grants, approvals, capability audit metadata

No Python worker, web frontend, daemon, or public network API is present. No
telemetry is collected by this application.
```

## Implemented and verified

| Area | Status | Evidence |
|---|---|---|
| Single Rust application | Implemented | One Cargo binary and Rust source tree |
| Local configuration | Implemented | TOML defaults and absolute database-path validation |
| SQLite persistence | Implemented | Versioned schema, foreign keys, busy timeout, workspace-scoped tasks |
| Provider abstraction | Implemented | Shared `ModelProvider`, typed requests/responses, capabilities, usage, tool-call data |
| OpenAI-compatible adapter | Implemented, mock-tested, live-tested with local Ollama compatibility endpoint | Model discovery, streamed Chat Completions, optional auth, tool/schema wire formats; no vendor cloud credentials live-tested |
| Ollama adapter | Implemented, mock-tested, live-tested | Model discovery and streamed chat for local `qwen3.5:0.8b` |
| Anthropic adapter | Implemented, mock-tested | Native model discovery, Messages API authentication, streaming text/usage/tool-call parsing; no real credentials tested |
| Gemini adapter | Implemented, mock-tested | Native model discovery, API-key header, streaming text/usage/tool-call parsing; no real credentials tested |
| OpenAI and OpenRouter | Implemented via OpenAI-compatible adapter, mock-tested | No OpenAI or OpenRouter credentials/live endpoint tested |
| Provider CLI/setup | Implemented, mock-tested | Setup, list/add/remove, discovery checks, and consented minimal inference tests; provider test is opt-in |
| Azure OpenAI and Codex | Not implemented | Azure deployment-specific API is not treated as generic OpenAI-compatible; Codex remains a separate external-agent runtime |
| Interactive terminal chat | Implemented | Ratatui transcript, streaming multi-turn context, model and permission commands, in-memory-only history, bounded workspace actions, and a Venus-inspired star/orbit brand mark |
| Model interaction metadata | Implemented | Provider/model/token counts/timestamp persisted, no prompt or response content |
| Capability contracts | Implemented | Versioned IDs, declared operations, effects, read-only/mutating classification, lifecycle state, schemas, requirements and dependencies |
| Schema validation | Implemented, limited subset | Rejects unsupported keywords; validates input, output, and capability configuration shapes |
| Permission profiles | Implemented and consumed by the bounded runtime | Open, Controlled, Restricted; profile state in SQLite; existing databases default to Open |
| Grants and revocation | Implemented | Exact capability/version/operation scope, required permission IDs, optional expiry/approval; revocation invalidates approvals |
| Approval workflow | Implemented for workspace capabilities | Single-request, five-minute approval, profile/grant/version/input bound; user CLI and chat approve/deny, CLI revoke; writes always require exact approval |
| Capability audit records | Implemented, metadata-only | Proposed/denied/approved/started/completed/failed/cancelled events with reasons and safe argument summaries |
| Bounded orchestrator | Implemented for bounded workspace operations | Displays effects, validates request/output, evaluates policy, rechecks authorization before execution, and records outcomes |
| Workspace summary, read, search, and exact replacement | Implemented | Honors visibility rules; reads UTF-8 files up to 256 KiB; search is bounded; one unique single-line replacement needs a write grant and fresh approval |
| Permission management CLI | Implemented | `permission profile`, `grant`, `grants`, `revoke`, `approvals`, `approve`, `deny`, `revoke-approval`, `audit` |
| User-configurable task retention | Implemented | Optional `task_retention_days`; removes old completed tasks only and reports removals |
| Workspace inspection | Implemented | Ignore-aware file inventory and top-level manifest detection |
| Git status | Implemented, limited | Fixed `git status --short --branch`; no arbitrary command input |
| Tests | Implemented | Unit tests for configuration, scanning, schema, and task behavior |
| Cross-platform CI | Implemented | Linux, macOS, and Windows workflow |
| Source-available license | Declared | Custom noncommercial attribution license metadata and text; dependency license compatibility has not been audited |

## Requirement gap analysis

| Requirement | Status | Current evidence or gap |
|---|---|---|
| Rust-owned core | Partial | CLI, config, bounded capability runtime, workspace scan, and storage exist; no public API, scheduler, or event system |
| Optional Python integrations | Missing | No worker or versioned inter-process protocol |
| Browser UI | Missing | The interactive interface is a native terminal UI; there is no web frontend |
| Provider independence | Partial | Ollama, OpenAI-compatible, Anthropic, and Gemini adapters implement the shared interface; real cloud accounts are not live-verified |
| Cloud providers | Partial | OpenAI/OpenRouter use the compatible protocol; Anthropic/Gemini have native adapters. Deterministic mock tests pass; no cloud credentials/live vendor endpoints tested |
| Local inference | Partial | Ollama HTTP protocol is mock-tested and live-tested with `qwen3.5:0.8b`. LM Studio/llama.cpp/vLLM not tested |
| Model configuration and routing | Partial | Multiple provider/model entries, aliases, and explicit model/provider selection; no automatic routing or fallback |
| Extensible capabilities | Partial | Built-in Rust executor contract and registry exist; no dynamic registration, install/remove lifecycle, Python worker, or distribution mechanism |
| Permission enforcement | Partial | The bounded runtime consumes profile/grant decisions for summary/read/search; mutation requires write grant and exact approval in every profile; no OS-specific resource controls |
| Agent loop and approvals | Partial | Human-supplied capability requests support exact approval and recheck; provider tool calls are not connected and there is no autonomous loop |
| Verification reports | Missing | No test/build execution or evidence report |
| Secure secret storage | Partial | Optional environment-variable key references; no OS keychain integration |
| Persistent task history | Partial | Descriptive tasks persist; no agent sessions or recovery |
| Audit and diagnostics | Partial | Capability proposal/decision/execution events persist locally; no tamper protection or audit retention setting |
| User-selected execution profiles | Partial | Profile setting is persistent and enforced for bounded reads; writes require a grant and approval under every profile |
| Logging controls | Partial | `log_level = "off"` disables tracing; task and model metadata retention are configurable; capability audit history has no retention setting |
| Project governance | Partial | Custom noncommercial attribution license, CI, contribution and security policies exist; release process, SBOM, and dependency-license audit remain |
| Cross-platform validation | Partial | CI is configured for three operating systems; local verification only covers the current host |

No open architectural decision is blocking the current reliability milestone. That said, I want the provider protocol, credential handling, and capability ABI decisions written down before those milestones start.

## Security and data-flow model

- Workspace inventory runs locally and doesn't transmit file contents. File-read/search content comes back only after you take an explicit action, and it's held in memory until a later prompt sends it to the selected provider.
- SQLite configuration and task state live in your operating-system data directories. Model interaction metadata is recorded without the prompt or response bodies.
- Interactive chat history is in memory only, not in SQLite. The current conversation context goes to the provider you explicitly selected on each request, with no automatic cloud fallback. There's no telemetry, account, hosted backend, or capability marketplace.
- Task data is isolated by canonical workspace path and SQL parameters.
- Completed task records are kept indefinitely unless you set `task_retention_days`; that setting never prunes pending work.
- Workspace inspection runs a fixed Git subcommand, not a shell command you supply. Git can still honor repository-local configuration like filters, so this is for trusted repositories only.
- There's no general filesystem mutation, command tool, model-driven action loop, sandbox, dynamic capability loader, or secure credential store. Workspace access is bounded, and the only write is an exact single-line replacement that needs a grant and approval.

So treat this as a local utility foundation, not a security sandbox or an autonomous agent.

## User sovereignty and execution profiles

The authority model I'm aiming for is user-owned and local: no Venus Agent account, subscription, hosted control plane, or developer approval. Venus must never silently elevate OS privileges. And its source-level rules can't restrict an owner who can modify the source or just operate the machine directly, so I don't pretend otherwise.

You choose among the implemented profiles for the bounded workspace capabilities:

| Profile | Implemented policy behavior for the bounded read-only executor |
|---|---|
| **Open** | No Venus Agent approval gate for available, schema-valid read-only requests. Mutating requests still require a matching grant and fresh exact approval. The process's actual OS permissions remain authoritative. |
| **Controlled** | Requires a matching capability/version/operation/permission grant. Mutating requests always require approval; read-only grants can additionally require one. |
| **Restricted** | Requires a matching grant and a new one-request user approval for every request. This is application policy, not OS isolation. |

The active profile shows up through `venus-agent permission profile`, and initialization and profile changes explain their consequences. In every profile, model output and other external content are untrusted data, not policy instructions. Open is the default. The bounded orchestrator uses these policies for summary, file-read, search, and exact text replacement. See [capabilities-and-permissions.md](capabilities-and-permissions.md) for the precise rules. These application rules are not a boundary against the machine owner, and they're not a substitute for OS permissions.

## Provider and local-model support

See [providers.md](providers.md) for configuration, the exact tested features, the data flow, and the manual integration steps. The cloud providers pass deterministic mocked protocol tests only; no real cloud credentials went into this implementation. The OpenAI-compatible adapter has been exercised against Ollama's compatibility endpoint, not a cloud provider. There's no automatic routing or fallback. Multimodal inputs, Azure OpenAI, and direct Codex authentication are all still unsupported.

## Capability development and installation

You can't install third-party capabilities as executable integrations right now. The versioned executor contract and the policy enforcement exist for trusted built-in Rust code only. There's no loader, manifest protocol, dependency resolver, authenticity check, Python worker, or sandbox. So don't drop executable plugins into this repository expecting them to load. A capability's declared permissions never grant it any authority on their own; you have to configure a grant and policy has to evaluate it.

## Prioritized remaining work

1. Provider hardening: add live, opt-in Ollama and compatible-endpoint integration runs, timeout/auth/stream protocol edge-case tests, and configured metadata retention.
2. Provider verification: live, opt-in vendor inference checks with real user credentials, plus protocol-specific pagination, rate-limit, and capability-metadata improvements.
3. Model orchestration: connect model-proposed tool calls to explicit requests, but only after defining turn limits, cancellation behavior, argument presentation, and safe handling of untrusted model output.
4. Coding capabilities: bounded reading/search and one exact single-line replacement with diff preview and approval are in; next come broader proposed edits and diffs. Keep the Venus terminal mark and the active-model/permission indicators as the interface grows. Leave arbitrary shell execution out of scope until its permission and OS boundaries are tested.
5. Controlled agent loop: connect model tool calls only with explicit policies, turn limits, cancellation, verification, and recovery, and report success only from outcomes actually observed.
6. Session persistence: define explicit opt-in, retention, and deletion behavior before storing any conversation content.
7. Python integration: add an optional versioned worker protocol only if a concrete integration needs it.
8. Distribution: package releases, produce an SBOM, audit dependency licenses, and document migrations.

The rule I'm holding myself to: each milestone needs integration and failure-path tests before the next one gains any authority or autonomy.

## Current capability setup

Build and run the implemented core with Rust:

```sh
cargo test --locked
cargo run -- init
cargo run -- inspect
cargo run -- capability list
cargo run -- capability run workspace.summary summarize --workspace .
cargo run -- capability run workspace.file-read read --input '{"path":"src/main.rs"}'
cargo run -- capability run workspace.search search --input '{"query":"TODO","case_sensitive":false}'
```

The inventory, task, and bounded read-only capability commands don't need a model. Provider commands need explicit configuration; see [providers.md](providers.md). There's no capability installation.
