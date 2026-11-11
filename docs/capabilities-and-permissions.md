# Capability contracts and permission profiles

Venus Agent includes provider-independent capability contracts, an in-memory registry, a bounded orchestrator, permission policy, request-scoped approvals, and SQLite audit history. Its executable capability set contains bounded, read-only workspace summary/read/search operations and one narrowly-scoped mutating operation: exact single-line text replacement. Model-generated tool calls are not connected to this runtime; there is no dynamic plugin loader, shell capability, general filesystem mutation, or autonomous agent loop.

## Trust and request lifecycle

The layers have separate responsibilities:

1. **Provider** generates model output. Provider identity never grants a capability.
2. **Orchestrator** accepts an explicit `CapabilityRequest`, displays its declared effects, checks policy, and invokes an eligible executor.
3. **Capability contract** describes declared operations, effects, whether an operation is read-only or mutating, input/output/configuration schemas, lifecycle state, dependencies, and required permissions.
4. **Permission policy** validates the request against the registered descriptor, profile, active version-bound grants, and any exact-request approval.
5. **Executor** runs only after a fresh authorization decision. Mutating workspace capabilities must declare `workspace.write`, have an active user grant, and receive fresh exact-request approval in every profile; operating-system permissions remain authoritative.

Model text, web content, files, arguments, and third-party data remain untrusted. Schema validation checks structure and types, not intent or safety.

## Capability contract

`src/capability.rs` defines:

- Stable identifiers (lowercase letters, digits, `.`, `_`, `-`) and numeric `major.minor.patch` versions.
- Name and description.
- Input, output, and optional capability-configuration schemas.
- Declared operation identifiers and user-readable effect descriptions.
- An explicit read-only/mutating effect classification; missing serialized classifications default to mutating.
- `available`, `disabled`, and `unavailable` lifecycle states.
- Versioned dependency declarations.
- Permission requirements with descriptions and a sensitivity flag.
- Registry registration, lookup, deterministic discovery, and state updates.

The schema validator supports a deliberately small JSON Schema subset: `type`, `description`, `enum`, `properties`, `required`, `items`, and boolean `additionalProperties`. Unsupported schema keywords are rejected instead of silently ignored. Schemas use `serde_json::Value`; this is structural validation, not a complete JSON Schema implementation.

The registry rejects duplicate capability identifiers. It remains in-memory, and only trusted Rust code built into Venus Agent is registered. There is no dynamic registration protocol, install/update/remove workflow, Python worker, or external capability loader. Dependencies are declared but not resolved or installed.

The bounded runtime registers these operations:

- `workspace.summary summarize` scans the selected, canonicalized workspace, honors ignore rules and skips common generated directories, reports file counts and top-level manifest names, and stops after 100,000 files. It does not return file contents.
- `workspace.file-read read` reads one visible UTF-8 file. Absolute paths, `..` path components, symbolic links, hidden/ignored/generated files, non-files, and paths outside the workspace are rejected. The maximum file size is 256 KiB; the visibility scan stops after 100,000 files. Returned workspace-relative paths use `/` on every platform; filesystem operations retain native paths.
- `workspace.search search` performs literal text search over visible UTF-8 files, skipping binary content. It scans at most 100,000 files and 16 MiB, returns at most 50 matches, and truncates each result line to 512 bytes. Returned workspace-relative paths use `/` on every platform.
- `workspace.file-replace replace` previews/applies one unique exact single-line replacement in a visible UTF-8 file up to 256 KiB. It binds approval to the current file digest and requested old/new text, rejects stale content, and atomically replaces the file only after explicit approval.

The ignore walker may read applicable ignore-rule files to determine which paths to skip. Workspace paths and request inputs are bound to approval fingerprints so an approval cannot be reused for a different action; raw request values and file contents are not stored in audit records. In the interactive chat, a file or search result remains in memory and is sent to the selected provider only when the user sends a subsequent prompt.

```sh
venus-agent capability list
venus-agent capability run workspace.summary summarize --workspace .
venus-agent capability run workspace.file-read read --workspace . \
  --input '{"path":"README.md"}'
venus-agent capability run workspace.search search --workspace . \
  --input '{"query":"TODO","case_sensitive":false}'
```

In every profile, the replacement requires a `workspace.write` grant and an exact approval. Read-only capabilities follow the selected profile; in Controlled mode, a grant can require approval, and Restricted always requires it:

```sh
venus-agent permission profile controlled
venus-agent permission grant workspace.file-read 1.0.0 read \
  --permission workspace.read
venus-agent capability run workspace.file-read read --workspace . \
  --input '{"path":"README.md"}'
venus-agent permission grant workspace.file-replace 1.0.0 replace \
  --permission workspace.write
venus-agent capability run workspace.file-replace replace --workspace . \
  --input '{"path":"README.md","expected_sha256":"<sha256>","old_text":"old text","new_text":"new text"}'
venus-agent permission approvals
venus-agent permission approve 12
venus-agent capability run workspace.file-replace replace --workspace . \
  --input '{"path":"README.md","expected_sha256":"<same-sha256>","old_text":"old text","new_text":"new text"}' \
  --approval-id 12
```

## Profiles

The active profile is stored in the local SQLite database. A newly initialized or migrated database defaults to **Open**, not Restricted. Check or change it with:

```sh
venus-agent permission profile
venus-agent permission profile open
venus-agent permission profile controlled
venus-agent permission profile restricted
```

Changing the profile changes only that setting. It does not edit grants or operating-system permissions. Pending/approved requests bound to a different profile are expired. The CLI explains the selected profile before applying the change.

| Profile | Venus policy behavior |
|---|---|
| **Open** | Adds no Venus Agent permission or approval gate for read-only capabilities. Mutating capabilities still require a matching grant and fresh exact-request approval. Open does not elevate privileges and is not a sandbox. |
| **Controlled** | Denies requests without an active grant matching capability ID, exact version, exact operation, and every declared permission requirement. Mutating capabilities additionally require approval; a read-only grant can also require approval. |
| **Restricted** | Requires the same explicit matching grant as Controlled and requires a fresh, request-specific user approval for every request. This is an application policy, not stronger OS isolation; isolation is not implemented. |

`disabled` and `unavailable` capabilities are not authorized in any profile. Disabled is an explicit capability lifecycle/configuration choice; unavailable means the integration cannot currently operate. In Open, Venus does not impose grant or approval gates on an available capability.

Manage grants without editing the provider TOML:

```sh
venus-agent permission grant example.reader 1.0.0 read \
  --permission data.read \
  --requires-approval \
  --expires-in-seconds 3600
venus-agent permission grants
venus-agent permission revoke 1
```

Grants are scoped to one capability version and one exact operation; wildcard grants are not supported. Permission identifiers must cover all requirements declared by the descriptor. Grants do not install or activate capabilities. Revoking a grant invalidates associated pending and approved requests.

## Approval primitives

An approval record binds:

- Capability ID and version.
- Exact operation.
- A SHA-256 fingerprint of the complete descriptor and request input.
- A structural argument summary (sanitized object field names are shown; values are omitted).
- Required permission identifiers and capability-declared consequences.
- The grant and profile that produced the request.
- `single_request` scope and a five-minute expiry.

The user, not the model, resolves an approval:

```sh
venus-agent permission approvals
venus-agent permission approve 12
venus-agent permission deny 12
venus-agent permission revoke-approval 12
```

Approvals become unusable when denied, revoked, expired, consumed, when their grant is revoked/expired, or when the profile changes. An approved request must be revalidated against the current descriptor and grant before policy returns `Allowed`; the approval is then consumed and cannot authorize a different request or be reused.

`capability run` presents the action, operation, workspace, effect class, effects, safe argument summary, and required permissions before policy evaluation. In Controlled or Restricted mode, when approval is required, the command persists a pending request and prints its ID; the user reviews and approves it separately, then reruns the same request with `--approval-id`. The orchestrator revalidates the exact request and current grant/profile immediately before invoking the capability. For an allowed request, the final policy check, approval consumption, and `started` audit event are committed together before execution, preventing a grant/profile change between those checks from authorizing a stale request. Open adds no approval prompt; it still displays the proposed action and proceeds only when the process and OS permit it. Model-proposed tool calls are not currently converted to capability requests.

## Audit records and data handling

Inspect metadata-only events with:

```sh
venus-agent permission audit --limit 50
```

Audit events distinguish `proposed`, `denied`, `approved`, `started`, `completed`, `failed`, and `cancelled`, and include the profile, decision/reason, capability/version, operation, approval reference, safe argument summary, timestamp, and a bounded error classification. Invalid lifecycle transitions are rejected. The workspace-summary executor emits start and terminal events; other capability execution remains unsupported.

Raw input values, prompts, and model responses are not written to capability approvals or audit rows. The SHA-256 request fingerprint is persisted to enforce exact-request matching; it is not an authorization secret and may allow guessing low-entropy inputs to someone who can read the local database. Use care with sensitive capability arguments. The safe summary shows sanitized field names and a sanitized workspace basename, not its full path or other values.

Grant, approval, profile, and audit data is local SQLite state. Existing database schema versions are migrated to version 3; old databases receive the Open profile and empty capability tables without losing existing workspace, task, or provider metadata. The provider TOML format is unchanged. Audit history currently has no retention/pruning setting and is retained until the user removes local database records. It is not tamper-proof against the machine owner.

## Security boundaries and remaining work

- Application-level profiles are policy, not a security boundary against a user who controls the machine or can modify Venus.
- Venus never elevates OS privileges. Future executors must run with the actual process permissions and handle OS denials explicitly.
- Open must remain free of Venus Agent approval gates; it must never be presented as protected isolation.
- Controlled and Restricted currently gate capability/version/operation/permission IDs, not fine-grained resource boundaries. There are no network, process, or OS isolation policies.
- Authorization is rechecked immediately before the current read-only executor is invoked. There is no general in-flight revocation mechanism for future long-running capabilities.
- Registry descriptors and permission requirements do not prove that an implementation behaves as declared. There is no plugin authenticity, code identity, dependency resolver, or process isolation.
- Future capabilities must treat inputs and external content as untrusted, validate semantics in addition to shape, avoid leaking secrets through errors/logs, and never treat model output or prior approvals as user authorization.

Next, connect model-proposed tool calls to explicit user-reviewed requests only after defining turn limits, cancellation behavior, argument presentation, and safe handling of untrusted model output. Do not add general file mutation or shell execution until revocation, isolation, audit retention, and OS-boundary behavior are tested.
