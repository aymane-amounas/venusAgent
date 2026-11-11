# Architecture, current status, and implementation plan

This document records what the repository implements today, not the intended end state.

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

No unresolved architectural decision prevents completing the current reliability milestone. Provider protocol, credential handling, and capability ABI decisions should be recorded before those milestones begin.

## Security and data-flow model

- Workspace inventory runs locally and does not transmit file contents. File-read/search content is returned only after an explicit user action and is held in memory until a subsequent prompt sends it to the selected provider.
- SQLite configuration and task state are stored in the user's operating-system data directories. Model interaction metadata is recorded without prompt or response bodies.
- Interactive chat history is held in memory only, not saved in SQLite. The current conversation context is sent to the explicitly selected provider with each request; there is no automatic cloud fallback. No telemetry, account, hosted backend, or capability marketplace exists.
- Task data is isolated by canonical workspace path and SQL parameters.
- Completed task records are retained indefinitely unless the user configures `task_retention_days`; pending work is never pruned by that setting.
- Workspace inspection invokes a fixed Git subcommand, not a user-provided shell command. Git may still honor repository-local configuration such as filters; inspection is therefore for trusted repositories only.
- There is no general filesystem mutation, command tool, model-driven action loop, sandbox, dynamic capability loader, or secure credential store. Workspace access is bounded; the only write is an exact single-line replacement requiring a grant and approval.

This is a local utility foundation, not a security sandbox or an autonomous agent.

## User sovereignty and execution profiles

The intended authority model is user-owned and local: no Venus Agent account, subscription, hosted control plane, or developer approval is required. Venus Agent must never silently elevate OS privileges. Its source-level rules cannot restrict an owner who can modify the source or operate the machine directly.

The user can choose the implemented profiles for bounded workspace capabilities:

| Profile | Implemented policy behavior for the bounded read-only executor |
|---|---|
| **Open** | No Venus Agent approval gate for available, schema-valid read-only requests. Mutating requests still require a matching grant and fresh exact approval. The process's actual OS permissions remain authoritative. |
| **Controlled** | Requires a matching capability/version/operation/permission grant. Mutating requests always require approval; read-only grants can additionally require one. |
| **Restricted** | Requires a matching grant and a new one-request user approval for every request. This is application policy, not OS isolation. |

The active profile is visible through `venus-agent permission profile`; initialization and profile changes explain consequences. Regardless of profile, model output and other external content are untrusted data, not policy instructions. Open is the default. The bounded orchestrator consumes these policies for summary, file-read, search, and exact text replacement. See [capabilities-and-permissions.md](capabilities-and-permissions.md) for precise rules. These application rules are not a boundary against the machine owner or a substitute for OS permissions.

## Provider and local-model support

See [providers.md](providers.md) for configuration, exact tested features, data flow, and manual integration steps. Cloud providers currently pass deterministic mocked protocol tests only; no real cloud credentials were used for this implementation. The OpenAI-compatible adapter was previously exercised against Ollama's compatibility endpoint, not a cloud provider. There is no automatic routing or fallback. Multimodal inputs, Azure OpenAI, and direct Codex authentication remain unsupported.

## Capability development and installation

Third-party capabilities cannot currently be installed as executable integrations. The versioned executor contract and policy enforcement are present for trusted built-in Rust code only, but there is no loader, manifest protocol, dependency resolver, authenticity check, Python worker, or sandbox. Do not place executable plugins in this repository expecting them to load. A capability's declared permissions never grant it authority; the user must configure a grant and policy must evaluate it.

## Prioritized remaining work

1. **Provider hardening:** add live, opt-in Ollama and compatible endpoint integration runs, timeout/auth/stream protocol edge-case tests, and configured metadata retention.
2. **Provider verification:** live, opt-in vendor inference checks with user credentials; protocol-specific pagination, rate-limit and capability metadata improvements.
3. **Model orchestration:** connect model-proposed tool calls to explicit requests only after defining turn limits, cancellation behavior, argument presentation, and safe handling of untrusted model output.
4. **Coding capabilities:** bounded workspace reading/search and one exact single-line replacement with diff preview and approval are implemented; next add broader proposed edits and diffs. Preserve the Venus terminal mark and active-model/permission indicators as the interface grows. Keep arbitrary shell execution outside scope until its permission and OS boundaries are tested.
5. **Controlled agent loop:** connect model tool calls only with explicit policies, turn limits, cancellation, verification, and recovery; report success only from observed outcomes.
6. **Session persistence:** define explicit opt-in, retention, and deletion behavior before storing conversation content.
7. **Python integration:** add an optional versioned worker protocol if a concrete integration requires it.
8. **Distribution:** package releases, produce an SBOM, audit dependency licenses, and document migrations.

Each milestone needs integration and failure-path tests before the next gains authority or autonomy.

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

The inventory, task, and bounded read-only capability commands do not require a model. Provider commands require explicit configuration; see [providers.md](providers.md). No capability installation is available.
