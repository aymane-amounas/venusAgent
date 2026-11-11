# Provider setup and verification

The CLI currently supports the following integrations through the Rust `ModelProvider` interface:

| Integration | Implementation | Automated tests | Live credentials/inference verified for this integration |
|---|---|---|---|
| Ollama | Native `/api/tags` discovery and streamed `/api/chat` | Deterministic local HTTP tests | Yes: `qwen3.5:0.8b` (2026-11-10), `qwen3.5:2b` and `qwen2.5-coder:3b` (2026-11-11) |
| OpenAI | OpenAI-compatible `/models` and streamed `/chat/completions` | Deterministic local HTTP tests | No OpenAI credentials or endpoint tested |
| OpenRouter | OpenAI-compatible `/models` and streamed `/chat/completions` | Same adapter's deterministic local HTTP tests | No OpenRouter credentials or endpoint tested |
| Anthropic | Native `/v1/models` and streamed `/v1/messages` | Deterministic local HTTP tests | No Anthropic credentials or endpoint tested |
| Google Gemini | Native `/v1beta/models` and streamed `:streamGenerateContent` | Deterministic local HTTP tests | No Google credentials or endpoint tested |
| Azure OpenAI | Not implemented | No | Unsupported |
| Codex CLI | Separate coding-agent runtime, not a model-provider adapter | Not applicable | Not integrated |

Compatibility depends on the selected server and model. Capability metadata is provider-reported or unknown; it is not proof that a selected model behaves correctly. If a provider returns a tool call, Venus Agent reports it but does not execute that model-proposed call. The CLI exposes bounded workspace summary/read/search and a single-line exact replacement capability that always requires a write grant and approval; there is no model-driven agent loop.

## Configure providers and models

On first launch, `venus` checks the default Ollama endpoint and lists installed models. When available, it automatically chooses an installed model, preferring a model ID containing `coder` or `code`, and configures it as `ollama-local` with a generated model alias. It does not ask for provider names or model aliases and never downloads a model. If Ollama is unavailable or has no models, setup offers cloud providers or lets the user finish later.

Run `venus setup` to add a provider or to repair the known previous-wizard naming mistake (provider name equals model ID and model alias is numeric). That repair is opt-in, makes a timestamped config backup, and normalizes names to `ollama-local` and a descriptive generated alias. Other existing provider/model names are preserved. `venus provider add` starts the same guided provider workflow.

Cloud presets use these environment-variable names:

| Provider | API key environment variable | API/model integration |
|---|---|---|
| OpenAI | `OPENAI_API_KEY` | OpenAI-compatible Chat Completions |
| Anthropic | `ANTHROPIC_API_KEY` | Native Anthropic Messages API |
| Google Gemini | `GEMINI_API_KEY` | Native Gemini API |
| OpenRouter | `OPENROUTER_API_KEY` | OpenAI-compatible Chat Completions |

Set the key in the shell before adding a cloud provider. Venus validates authentication and discovers models using that provider's own API. After model selection, it asks before sending one short inference test; provider usage or charges may apply. Configuration is saved only after successful inference. Only the environment-variable name is stored; key values never enter TOML. Cloud setup may stop with an actionable missing-credential message rather than silently saving an unvalidated provider.

Providers configured with an API-key environment variable must use HTTPS for non-loopback endpoints. HTTP is allowed for loopback endpoints such as local test servers; do not use plaintext HTTP for a remote provider.

No automatic failover is performed. Requests, including any workspace context the user explicitly supplied, go only to the selected provider. If it is unavailable, Venus reports the error and waits for the user to switch or retry.

For manual configuration, run `venus-agent init` to create the TOML config and print its exact path, then edit that `config.toml`. Provider and model names are user-chosen aliases; `model_id` is sent unchanged, allowing custom model identifiers.

```toml
database_path = "/absolute/path/to/state.sqlite3"
log_level = "info"
model_metadata_retention_days = 90

[[providers]]
name = "cloud"
kind = "openai_compatible"
base_url = "https://api.openai.com/v1"
api_key_env = "OPENAI_API_KEY"
timeout_secs = 120

[[providers]]
name = "ollama-local"
kind = "ollama"
base_url = "http://127.0.0.1:11434"
timeout_secs = 120

[[models]]
name = "cloud-default"
provider = "cloud"
model_id = "gpt-4o-mini"
task = "general"

[[models]]
name = "local-coder"
provider = "ollama-local"
model_id = "qwen2.5-coder:7b"
task = "coding"
```

An API key is optional for OpenAI-compatible endpoints. If `api_key_env` is configured, the named environment variable must exist when making requests. Set it in the shell that launches the CLI, not in TOML:

```sh
export OPENAI_API_KEY="your-provider-key"
venus-agent provider check cloud
venus-agent provider models cloud
venus-agent provider test cloud --model cloud-default
venus-agent ask --provider cloud --model cloud-default "Explain what a Rust ownership move is."
```

The CLI never silently falls back to another provider. Select a different provider explicitly with `--provider ollama-local`. `--model` accepts either a configured model alias for that provider or a raw model ID.

The OpenAI-compatible adapter appends `/models` and `/chat/completions` to the configured base URL. For OpenAI, include `/v1`; for compatible services, use that service's documented API root. No vendor-specific OpenAI API integration is implied by compatibility mode.

## Manage configured providers

The CLI preserves the existing `[[providers]]` and `[[models]]` TOML structure:

```sh
venus provider list
venus provider add
venus provider check openai
venus provider models openai
venus provider test openai --model openai-gpt-4o-mini
venus provider switch openai openai-gpt-4o-mini
venus provider remove openai --yes
```

`check` verifies connectivity/authentication and model discovery. `test` makes a real minimal inference request with the selected configured alias or model ID; it can incur provider usage. `switch` moves a configured model to the first position, which persists it as the default for new chats. `remove` requires `--yes` and removes the provider plus its model entries; Venus prints which remaining model becomes first at the next launch.

In chat, `/providers` and `/models` show configured entries, `/provider test <name>` checks authentication/model discovery, and `/provider switch <name> <model>` switches for the current chat. `/model <provider> <alias-or-id>` remains supported. The first configured model is the default for new chats; use `venus provider switch` to change it persistently. There is no implicit provider fallback. `venus setup` is the interactive flow for adding providers; it retains existing configuration and only changes a configuration after explicit, successful setup.

## Azure OpenAI and Codex

Azure OpenAI is not currently supported. Its deployment-scoped URL, API-version, authentication-header, and model/deployment discovery requirements are not equivalent to this app's existing generic OpenAI-compatible `/models` and `/chat/completions` contract. Do not configure it as OpenAI-compatible and assume it works.

Codex CLI is an external coding-agent runtime, not a model API. OpenAI documents both `codex exec` for programmatic Codex CLI workflows and Codex CLI authentication via ChatGPT sign-in for eligible subscription access or an API key for usage-based access. These are Codex-managed modes, not interchangeable credentials for Venus. Venus does not reuse Codex's local auth state, perform undocumented OAuth, or proxy Codex as a model provider. Calling `codex exec` would delegate to a separate coding agent with its own execution and permission model; Venus has not integrated that external-agent boundary. See [OpenAI Codex authentication](https://developers.openai.com/codex/auth/), [Codex CLI](https://developers.openai.com/codex/cli/), and [Codex developer commands](https://developers.openai.com/codex/developer-commands/).

## Interactive terminal chat

After configuring a provider and model, run `venus` with no subcommand to start the Ratatui chat. The first configured model is selected initially. Use `/models` or `/model` to list configured models and `/model <provider> <alias-or-model-id>` to switch; existing commands such as `venus-agent ask` remain available.

The chat keeps role-tagged turns in memory for the current process and sends that context to the explicitly selected provider on each request. `/clear` removes the local in-memory transcript. Transcript content is not persisted; the existing metadata-only model interaction records are still written to SQLite. `/summary`, `/read <relative-path>`, and `/search <literal-query>` make bounded read-only workspace capabilities available in chat and use the configured permission profile. `/replace <path> <old text> => <new text>` previews one unique single-line replacement and will not write without a `workspace.write` grant and fresh exact approval. Read/search output is marked as untrusted context and is sent only with your next prompt. Model-proposed tool calls are not executed.

## Local Ollama

Install Ollama using its official instructions, start its local server, and pull a model. Venus defaults to IPv4 loopback (`127.0.0.1`) so existing `localhost` configurations continue to work when the server listens on IPv4:

```sh
ollama pull qwen2.5-coder:7b
venus-agent provider check ollama-local
venus-agent provider models ollama-local
venus-agent ask --provider ollama-local --model local-coder "Explain this model's main strengths."
```

Ollama requests do not require a cloud API key. The configured base URL receives the prompt; do not point it at an untrusted remote service if you expect local-only behavior. The configured local Ollama integration has been live-tested as recorded below.

## Custom compatible endpoints

Add another `[[providers]]` record with a distinct `name`, `kind = "openai_compatible"`, endpoint base URL, optional `api_key_env`, and timeout. This adapter targets the Chat Completions and Models endpoint shapes. Services with different schemas (including Anthropic and Gemini native APIs) are not supported by this adapter and need a dedicated implementation.

## Streaming, cancellation, timeout, and privacy

`venus-agent ask` and the interactive chat stream text to the terminal; Ctrl-C cancels an in-progress request. The client timeout applies to the whole response. Provider/model identity and available token counts are displayed; a metadata-only record (provider, model, token counts, timestamp) is persisted in the local SQLite database. Prompt and response text are not persisted. Metadata is retained indefinitely by default; configure `model_metadata_retention_days` to automatically prune older records.

Cloud providers receive the prompt and their own service terms apply. Locality is determined by the selected endpoint; Venus does not silently reroute requests. HTTP error bodies are returned to aid diagnosis and known configured API-key values are redacted. Provider endpoints may not embed credentials in URL user-info. Do not send secrets in prompts. Rate-limit responses include a retry/quota hint; automatic retries are not performed, avoiding surprise duplicate requests or costs.

Credentials should be supplied through environment variables. TOML supports only the *name* of the environment variable, not a credential value. Environment variables can be visible to processes with sufficient OS access; use the operating system's secret-management facilities when launching the CLI in higher-risk environments. No OS keychain integration is implemented yet.

## Manual integration checks

These are opt-in and require the user's configured service; they are not part of `cargo test`:

1. For a live provider, set its environment variable, run `venus provider check <name>`, `venus provider models <name>`, and `venus provider test <name> --model <alias-or-id>`.
2. For local inference, start Ollama, ensure the selected model is already pulled, then run `venus provider check <name>`, `venus provider models <name>`, and `venus provider test <name> --model <alias-or-id>`.
3. Record endpoint type, model ID, date, and observed results without recording credentials or prompt contents.

Do not treat mock HTTP tests as proof of compatibility with any particular commercial or local model.

## Live verification record

On 2026-11-10, the actual CLI was run against the user's running local Ollama service using an isolated temporary config/database:

- Native Ollama: connectivity check, model discovery, and a real streaming prompt through `/api/chat` succeeded for `qwen3.5:0.8b`.
- OpenAI-compatible adapter: connectivity check and a real streaming prompt through Ollama's `/v1/chat/completions` compatibility endpoint succeeded for the same model.
- Both responses reported provider alias, model ID, and token counts. Prompt and response content were not persisted.
- This establishes local parity across the two adapters, not compatibility with a cloud vendor's endpoint.
- No cloud provider was configured, and no cloud credentials were used. Live cloud integration remains unverified.
