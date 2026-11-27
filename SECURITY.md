# Security policy

## Supported versions

I only support the current default branch. There are no tagged releases yet, so once there are, this section will say which ones still get fixes.

## Reporting a vulnerability

Please don't open a public issue for an exploitable bug. Report it privately through **Security → Report a vulnerability** on this repository ([direct link](https://github.com/aymane-amounas/venusAgent/security/advisories/new)); private reporting is turned on. Keep exploit details out of public issues.

When you report, include the affected version or commit, the impact, how to reproduce it, and any fix you have in mind. Don't send real credentials, personal data, or anyone else's data.

## What's actually in scope

Here's what the CLI really does today, so you know what you're trusting. It takes a workspace inventory, runs one fixed Git status command, and stores local records: tasks, model-interaction metadata, capability grants, approval records, and capability audit metadata. It can send prompts to the endpoint you explicitly select (Ollama, an OpenAI-compatible service, Anthropic, or Gemini).

Workspace summary, read, and search are bounded and read-only. File reads are capped at 256 KiB and reject symlinks, hidden/ignored/generated files, and anything outside the workspace. Search scans at most 100,000 files and 16 MiB and returns at most 50 matches. There is exactly one mutating capability: it replaces one exact, unique single-line string in a visible UTF-8 file up to 256 KiB. It refuses stale content and always needs a `workspace.write` grant plus a fresh exact-request approval, even in Open profile. No model-generated actions and no third-party capabilities run. The ignore walker does read ignore-rule files to figure out what to skip.

One thing to be clear about: workspace inspection is not a sandbox. Git may honor repository-local configuration, including filters, so only run inspection on repositories you trust. The capability summary doesn't invoke Git and stops after 100,000 files, but none of this is a substitute for OS-level security.

## How user control is meant to work

The project does not require a Venus Agent account, a subscription, centralized approval, or a hosted backend. It must never silently raise operating-system privileges, and I won't describe software-level policy as a boundary against the person who owns the machine. You can always read and change the source.

The permission-profile setting and the policy primitives behind it are enforced by the bounded workspace capabilities. This is how the three profiles behave right now:

- **Open:** no Venus Agent permission or approval gate for available, schema-valid read-only requests. Mutating requests still need a matching grant and a fresh one-request approval. Everything still runs under the process's actual OS permissions.
- **Controlled:** denies any request without a matching capability/version/operation/permission grant. Mutating requests always need a one-request user approval, and read-only grants can require one too.
- **Restricted:** needs a matching grant and a new one-request user approval for every request. OS isolation is not implemented.

The profile defaults to Open, lives in the local database, and you inspect or change it with `venus-agent permission profile`. Changing it explains the consequences, keeps your grants, and expires approvals that were bound to another profile. You can inspect grants, approvals, and audit events from the CLI. Model output, websites, files, scripts, and any future capabilities stay untrusted input in every profile. Open mode uses only the process's real OS permissions; it's not a sandbox and must not elevate anything.

There is no shell execution, no general filesystem mutation, no model-driven agent loop, no dynamic plugin loader, and no arbitrary capability execution. The only write is the constrained exact replacement above. File and search results are held in memory and go only to the provider you selected, and only when you send a following prompt; a provider failure never falls back to another service. Prompt, response, and file content are not persisted. API keys are read from the environment variables you configure, never written to TOML, and redacted from provider error bodies; keyed non-loopback providers have to use HTTPS, and HTTP is allowed only for loopback. There's no OS keychain integration. An approval primitive never turns a model's statement into your consent.

On the audit log: each row carries a SHA-256 hash chained to the previous row, so editing, deleting, or reordering earlier rows breaks the chain and `venus-agent permission audit --verify` will catch it. That's tamper-evidence, not tamper-proofing. The hash and its inputs live in the same database, so anyone who can write the file can recompute the chain forward from their edit. If you want to detect that too, keep an external copy of the head hash that `--verify` prints and compare it. Audit rows hold only metadata and a shape summary, but the persisted request fingerprint is an unkeyed hash, so someone who can read the local database could guess low-entropy inputs. Audit retention is indefinite right now and not configurable. `log_level = "off"` turns off tracing output, `task_retention_days` controls completed-task retention, and `model_metadata_retention_days` controls provider-metadata retention. Cloud prompts leave the machine for the provider you configured; Ollama stays local only when you point it at a local endpoint. Application-level policy is not a security boundary against the machine's owner.
