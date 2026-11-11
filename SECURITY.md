# Security policy

## Supported versions

Only the current default branch is supported until the project publishes tagged releases.

## Reporting a vulnerability

Please do not report exploitable vulnerabilities in a public issue. Report it privately through **Security → Report a vulnerability** on this repository ([direct link](https://github.com/aymane-amounas/venusAgent/security/advisories/new)); private vulnerability reporting is enabled. Do not include exploit details in a public issue.

Include the affected version or commit, impact, reproduction conditions, and any suggested mitigation. Do not include real credentials, personal data, or data belonging to another person.

## Current security scope

The current CLI performs workspace inventory, invokes a fixed Git status command, and stores local task records, model-interaction metadata, capability grants, approval records, and capability audit metadata. It can send prompts to the explicitly selected Ollama, OpenAI-compatible, Anthropic, or Gemini endpoint. Workspace summary/read/search are bounded and read-only; file reads are limited to 256 KiB and reject symlinks, hidden/ignored/generated files, and paths outside the workspace. Search scans at most 100,000 files and 16 MiB and returns at most 50 matches. One mutating capability replaces one exact unique single-line string in a visible UTF-8 file up to 256 KiB; it refuses stale content and always requires a `workspace.write` grant plus fresh exact-request approval, including in Open profile. No model-generated actions or third-party capabilities are executed. The ignore walker may read ignore-rule files.

Workspace inspection is not a sandbox. Git may honor repository-local configuration, including filters. Run inspection only on repositories you trust. The capability summary does not invoke Git and stops after 100,000 files, but application-level permission checks are not a substitute for OS-level security.

## User control principles

The project does not require a Venus Agent account, subscription, centralized approval, or hosted backend. It must not silently elevate operating-system privileges, and software-level policy must never be described as a boundary against the machine's owner. Users must be able to inspect and modify the source.

The permission-profile setting and policy primitives are enforced by bounded workspace capabilities. Their current behavior is:

- **Open:** no Venus Agent permission or approval gate for available, schema-valid read-only requests. Mutating requests still require a matching grant and fresh one-request approval. All operations remain subject to the process's actual OS permissions.
- **Controlled:** denies requests without a matching capability/version/operation/permission grant; mutating requests always require a one-request user approval, and read-only grants may also require one.
- **Restricted:** requires a matching grant and a new one-request user approval for every request. OS isolation is not implemented.

The profile defaults to Open, is persisted in the local database, and can be inspected or changed with `venus-agent permission profile`. Changes explain their consequences, preserve grants, and expire approvals bound to another profile. Grants, approvals, and audit events can be inspected from the CLI. External model output, websites, files, scripts, and future capabilities remain untrusted input in every profile. Open mode must use only the process's actual OS permissions; it is not a sandbox and must not elevate privileges.

There is no shell execution, general filesystem mutation, model-driven agent loop, dynamic plugin loader, or arbitrary capability execution. The only write operation is the constrained exact replacement described above. File and search results are held in memory and are sent only to the explicitly selected provider when the user submits a subsequent prompt; provider failure never triggers fallback to another service. Prompt, response, and file content are not persisted. API key values are read from configured environment variables, never saved in TOML, and redacted from provider error bodies; keyed non-loopback providers must use HTTPS. HTTP is allowed for loopback endpoints. No OS keychain integration is implemented. Approval primitives do not make a model statement user consent. Audit rows contain only metadata and a shape summary, but the persisted request fingerprint is an unkeyed hash and can enable guessing low-entropy inputs by someone who can read the local database. Audit retention is currently indefinite and not configurable. `log_level = "off"` disables tracing output, `task_retention_days` controls completed task retention, and `model_metadata_retention_days` controls provider metadata retention. Cloud prompts leave the machine for the configured provider; Ollama is local only when the user selects a local endpoint. Application-level policy is not a security boundary against the machine owner.
