# Provider setup and verification

These are the integrations the CLI supports through the Rust `ModelProvider` interface, and how far each one has actually been tested:

| Integration | Implementation | Automated tests | Live credentials/inference verified for this integration |
|---|---|---|---|
| Ollama | Native `/api/tags` discovery and streamed `/api/chat` | Deterministic local HTTP tests | Yes: `qwen3.5:0.8b` (2026-11-10), `qwen3.5:2b` and `qwen2.5-coder:3b` (2026-11-11) |
| OpenAI | OpenAI-compatible `/models` and streamed `/chat/completions` | Deterministic local HTTP tests | No OpenAI credentials or endpoint tested |
| OpenRouter | OpenAI-compatible `/models` and streamed `/chat/completions` | Same adapter's deterministic local HTTP tests | No OpenRouter credentials or endpoint tested |
| Anthropic | Native `/v1/models` and streamed `/v1/messages` | Deterministic local HTTP tests | No Anthropic credentials or endpoint tested |
| Google Gemini | Native `/v1beta/models` and streamed `:streamGenerateContent` | Deterministic local HTTP tests | No Google credentials or endpoint tested |
| Azure OpenAI | Not implemented | No | Unsupported |
| Codex CLI | Separate coding-agent runtime, not a model-provider adapter | Not applicable | Not integrated |

Whether a given model works depends on the server and model you pick. Capability metadata is either provider-reported or unknown, and it isn't proof that a model behaves correctly. If a provider returns a tool call, Venus reports it but doesn't execute it. The CLI exposes bounded workspace summary/read/search and a single-line exact replacement that always needs a write grant and approval. There's no model-driven agent loop.

## Configure providers and models

On first launch, `venus` checks the default Ollama endpoint and lists installed models. If there's one available, it picks it automatically, preferring a model ID with `coder` or `code` in the name, and configures it as `ollama-local` with a generated alias. It doesn't ask you for provider names or model aliases, and it never downloads a model. If Ollama isn't up or has no models, setup offers cloud providers or lets you finish later.

Run `venus setup` to add a provider, or to repair the one naming mistake an earlier version of the wizard could make (provider name equal to the model ID, with a numeric model alias). That repair is opt-in, makes a timestamped config backup first, and normalizes the names to `ollama-local` plus a descriptive generated alias. Any other provider or model names you have are left alone. `venus provider add` starts the same guided flow.

The cloud presets use these environment-variable names:

| Provider | API key environment variable | API/model integration |
|---|---|---|
| OpenAI | `OPENAI_API_KEY` | OpenAI-compatible Chat Completions |
| Anthropic | `ANTHROPIC_API_KEY` | Native Anthropic Messages API |
| Google Gemini | `GEMINI_API_KEY` | Native Gemini API |
| OpenRouter | `OPENROUTER_API_KEY` | OpenAI-compatible Chat Completions |

Set the key in your shell before adding a cloud provider. Venus validates authentication and discovers models through that provider's own API. After you pick a model, it asks before sending one short inference test, and provider usage or charges may apply. The configuration is saved only after that inference succeeds. Only the environment-variable name is stored; the key value never enters TOML. If a credential is missing, cloud setup stops with an actionable message instead of quietly saving a provider it couldn't validate.

Providers configured with an API-key environment variable have to use HTTPS for non-loopback endpoints. HTTP is allowed for loopback endpoints like local test servers; don't use plaintext HTTP for a remote provider.

There's no automatic failover. Requests, including any workspace context you explicitly supplied, go only to the selected provider. If it's down, Venus reports the error and waits for you to switch or retry.

To configure by hand, run `venus-agent init` to create the TOML config and print its exact path, then edit that `config.toml`. Provider and model names are aliases you choose; `model_id` is sent unchanged, so custom model identifiers work.

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

An API key is optional for OpenAI-compatible endpoints. If you set `api_key_env`, the named environment variable has to exist when requests are made. Set it in the shell that launches the CLI, not in TOML:

```sh
export OPENAI_API_KEY="your-provider-key"
venus-agent provider check cloud
venus-agent provider models cloud
venus-agent provider test cloud --model cloud-default
venus-agent ask --provider cloud --model cloud-default "Explain what a Rust ownership move is."
```

The CLI never silently falls back to another provider. Pick a different one explicitly with `--provider ollama-local`. `--model` takes either a configured model alias for that provider or a raw model ID.

The OpenAI-compatible adapter appends `/models` and `/chat/completions` to the base URL you configure. For OpenAI, include `/v1`; for compatible services, use that service's documented API root. Compatibility mode doesn't imply any vendor-specific OpenAI integration.

## Manage configured providers

The CLI keeps your existing `[[providers]]` and `[[models]]` TOML structure:

```sh
venus provider list
venus provider add
venus provider check openai
venus provider models openai
venus provider test openai --model openai-gpt-4o-mini
venus provider switch openai openai-gpt-4o-mini
venus provider remove openai --yes
```

`check` verifies connectivity, authentication, and model discovery. `test` makes a real minimal inference request with the alias or model ID you give it, so it can incur provider usage. `switch` moves a configured model to the first position, which makes it the default for new chats. `remove` requires `--yes` and removes the provider plus its model entries; Venus prints which remaining model becomes first at the next launch.

In chat, `/providers` and `/models` show your configured entries, `/provider test <name>` checks authentication and model discovery, and `/provider switch <name> <model>` switches for the current chat. `/model <provider> <alias-or-id>` still works. The first configured model is the default for new chats; use `venus provider switch` to change that for good. There's no implicit fallback. `venus setup` is the interactive flow for adding providers; it keeps your existing configuration and only changes anything after a successful setup.

## Azure OpenAI and Codex

Azure OpenAI isn't supported yet. Its deployment-scoped URL, API-version, authentication-header, and model/deployment discovery aren't the same as this app's generic OpenAI-compatible `/models` and `/chat/completions` contract, so don't configure it as OpenAI-compatible and assume it'll work.

Codex CLI is an external coding-agent runtime, not a model API. OpenAI documents `codex exec` for programmatic Codex CLI workflows, and Codex CLI authentication through ChatGPT sign-in for eligible subscriptions or an API key for usage-based access. Those are Codex-managed modes, not credentials Venus can borrow. Venus doesn't reuse Codex's local auth state, do any undocumented OAuth, or proxy Codex as a model provider. Calling `codex exec` would hand off to a separate coding agent with its own execution and permission model, and I haven't integrated that boundary. See [OpenAI Codex authentication](https://developers.openai.com/codex/auth/), [Codex CLI](https://developers.openai.com/codex/cli/), and [Codex developer commands](https://developers.openai.com/codex/developer-commands/).

## Interactive terminal chat

Once a provider and model are configured, run `venus` with no subcommand to start the Ratatui chat. The first configured model is selected to begin with. Use `/models` or `/model` to list configured models and `/model <provider> <alias-or-model-id>` to switch; commands like `venus-agent ask` are still there.

The chat keeps role-tagged turns in memory for the current process and sends that context to the selected provider on each request. `/clear` drops the local in-memory transcript. Transcript content isn't persisted; the metadata-only model interaction records still get written to SQLite. `/summary`, `/read <relative-path>`, and `/search <literal-query>` make the bounded read-only workspace capabilities available in chat and use the configured permission profile. `/replace <path> <old text> => <new text>` previews one unique single-line replacement and won't write without a `workspace.write` grant and a fresh exact approval. Read and search output is marked as untrusted context and is sent only with your next prompt. Model-proposed tool calls aren't executed.

## Local Ollama

Install Ollama with its official instructions, start the local server, and pull a model. Venus defaults to IPv4 loopback (`127.0.0.1`) so existing `localhost` configurations keep working when the server listens on IPv4:

```sh
ollama pull qwen2.5-coder:7b
venus-agent provider check ollama-local
venus-agent provider models ollama-local
venus-agent ask --provider ollama-local --model local-coder "Explain this model's main strengths."
```

Ollama requests don't need a cloud API key. The configured base URL receives the prompt, so don't point it at an untrusted remote service if you expect local-only behavior. The local Ollama integration has been live-tested, as recorded below.

## Custom compatible endpoints

Add another `[[providers]]` record with a distinct `name`, `kind = "openai_compatible"`, the endpoint base URL, an optional `api_key_env`, and a timeout. This adapter targets the Chat Completions and Models endpoint shapes. Services with different schemas, including the Anthropic and Gemini native APIs, aren't handled by this adapter and need a dedicated implementation.

## Streaming, cancellation, timeout, and privacy

`venus-agent ask` and the interactive chat stream text to the terminal; Ctrl-C cancels an in-progress request. The client timeout applies to the whole response. Provider/model identity and available token counts are shown, and a metadata-only record (provider, model, token counts, timestamp) is persisted in the local SQLite database. Prompt and response text aren't persisted. Metadata is kept indefinitely by default; set `model_metadata_retention_days` to prune older records automatically.

Cloud providers receive the prompt and their own terms apply. Locality comes down to the endpoint you select; Venus doesn't silently reroute requests. HTTP error bodies are returned to help you diagnose problems, and known configured API-key values are redacted. Provider endpoints may not embed credentials in the URL user-info. Don't put secrets in prompts. Rate-limit responses include a retry/quota hint, but automatic retries aren't performed, which avoids surprise duplicate requests or costs.

Supply credentials through environment variables. TOML holds only the *name* of the environment variable, never a credential value. Environment variables can be visible to processes with enough OS access, so use your operating system's secret-management facilities when launching the CLI in higher-risk environments. There's no OS keychain integration yet.

## Manual integration checks

These are opt-in, need your configured service, and aren't part of `cargo test`:

1. For a live provider, set its environment variable, then run `venus provider check <name>`, `venus provider models <name>`, and `venus provider test <name> --model <alias-or-id>`.
2. For local inference, start Ollama, make sure the model is already pulled, then run `venus provider check <name>`, `venus provider models <name>`, and `venus provider test <name> --model <alias-or-id>`.
3. Record the endpoint type, model ID, date, and what you observed, without recording credentials or prompt contents.

Don't treat mock HTTP tests as proof of compatibility with any particular commercial or local model.

## Live verification record

On 2026-11-10 I ran the actual CLI against my running local Ollama service, using an isolated temporary config and database:

- Native Ollama: connectivity check, model discovery, and a real streaming prompt through `/api/chat` all worked for `qwen3.5:0.8b`.
- OpenAI-compatible adapter: connectivity check and a real streaming prompt through Ollama's `/v1/chat/completions` compatibility endpoint worked for the same model.
- Both responses reported the provider alias, model ID, and token counts. Prompt and response content weren't persisted.
- That establishes local parity across the two adapters, not compatibility with a cloud vendor's endpoint.
- No cloud provider was configured and no cloud credentials were used. Live cloud integration is still unverified.
