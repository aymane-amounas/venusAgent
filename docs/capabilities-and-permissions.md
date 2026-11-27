# Capability contracts and permission profiles

Venus Agent ships provider-independent capability contracts, an in-memory registry, a bounded orchestrator, permission policy, request-scoped approvals, and SQLite audit history. The capabilities that can actually run are the bounded read-only workspace summary/read/search operations and one narrow mutating operation: exact single-line text replacement. Model-generated tool calls aren't connected to this runtime, and there's no dynamic plugin loader, shell capability, general filesystem mutation, or autonomous agent loop.

## Trust and request lifecycle

Each layer has one job, and I kept them separate on purpose:

1. The provider generates model output. Provider identity never grants a capability.
2. The orchestrator accepts an explicit `CapabilityRequest`, shows its declared effects, checks policy, and invokes an eligible executor.
3. The capability contract describes the declared operations, the effects, whether an operation is read-only or mutating, the input/output/configuration schemas, the lifecycle state, dependencies, and required permissions.
4. The permission policy validates the request against the registered descriptor, the profile, the active version-bound grants, and any exact-request approval.
5. The executor runs only after a fresh authorization decision. A mutating workspace capability has to declare `workspace.write`, have an active user grant, and get a fresh exact-request approval in every profile. OS permissions are still the ones that actually matter.

Model text, web content, files, arguments, and third-party data all stay untrusted. Schema validation checks structure and types, not intent or safety.

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

The schema validator supports a deliberately small JSON Schema subset: `type`, `description`, `enum`, `properties`, `required`, `items`, and boolean `additionalProperties`. Anything outside that gets rejected rather than silently ignored. Schemas use `serde_json::Value`, so this is structural validation, not a full JSON Schema implementation.

The registry rejects duplicate capability identifiers. It stays in memory, and the only things registered are trusted Rust built into Venus itself. There's no dynamic registration protocol, no install/update/remove workflow, no Python worker, and no external loader. Dependencies are declared but not resolved or installed.

The bounded runtime registers these operations:

- `workspace.summary summarize` scans the selected, canonicalized workspace, honors ignore rules and skips common generated directories, reports file counts and top-level manifest names, and stops after 100,000 files. It does not return file contents.
- `workspace.file-read read` reads one visible UTF-8 file. Absolute paths, `..` path components, symbolic links, hidden/ignored/generated files, non-files, and paths outside the workspace are rejected. The maximum file size is 256 KiB; the visibility scan stops after 100,000 files. Returned workspace-relative paths use `/` on every platform; filesystem operations retain native paths.
- `workspace.search search` performs literal text search over visible UTF-8 files, skipping binary content. It scans at most 100,000 files and 16 MiB, returns at most 50 matches, and truncates each result line to 512 bytes. Returned workspace-relative paths use `/` on every platform.
- `workspace.file-replace replace` previews/applies one unique exact single-line replacement in a visible UTF-8 file up to 256 KiB. It binds approval to the current file digest and requested old/new text, rejects stale content, and atomically replaces the file only after explicit approval.

The ignore walker reads the applicable ignore-rule files to figure out which paths to skip. Workspace paths and request inputs are bound to approval fingerprints, so an approval can't be reused for a different action, and raw request values and file contents never go into audit records. In the chat, a file or search result stays in memory and reaches the selected provider only when you send a following prompt.

```sh
venus-agent capability list
venus-agent capability run workspace.summary summarize --workspace .
venus-agent capability run workspace.file-read read --workspace . \
  --input '{"path":"README.md"}'
venus-agent capability run workspace.search search --workspace . \
  --input '{"query":"TODO","case_sensitive":false}'
```

In every profile, the replacement needs a `workspace.write` grant and an exact approval. The read-only capabilities follow whichever profile you've selected: in Controlled a grant can require approval, and Restricted always requires it:

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

The active profile lives in the local SQLite database. A freshly initialized or migrated database defaults to **Open**, not Restricted. Check or change it with:

```sh
venus-agent permission profile
venus-agent permission profile open
venus-agent permission profile controlled
venus-agent permission profile restricted
```

Changing the profile changes only that setting. It doesn't touch your grants or your OS permissions. Pending or approved requests that were bound to a different profile get expired. The CLI explains the profile you picked before it applies the change.

| Profile | Venus policy behavior |
|---|---|
| **Open** | Adds no Venus Agent permission or approval gate for read-only capabilities. Mutating capabilities still require a matching grant and fresh exact-request approval. Open does not elevate privileges and is not a sandbox. |
| **Controlled** | Denies requests without an active grant matching capability ID, exact version, exact operation, and every declared permission requirement. Mutating capabilities additionally require approval; a read-only grant can also require approval. |
| **Restricted** | Requires the same explicit matching grant as Controlled and requires a fresh, request-specific user approval for every request. This is an application policy, not stronger OS isolation; isolation is not implemented. |

`disabled` and `unavailable` capabilities aren't authorized in any profile. Disabled is a deliberate lifecycle/configuration choice; unavailable means the integration can't currently operate. In Open, Venus adds no grant or approval gate on an available capability.

Manage grants without editing the provider TOML:

```sh
venus-agent permission grant example.reader 1.0.0 read \
  --permission data.read \
  --requires-approval \
  --expires-in-seconds 3600
venus-agent permission grants
venus-agent permission revoke 1
```

Grants are scoped to one capability version and one exact operation; there are no wildcard grants. The permission identifiers have to cover every requirement the descriptor declares. A grant doesn't install or activate a capability. Revoking a grant invalidates the pending and approved requests tied to it.

## Approval primitives

An approval record binds:

- Capability ID and version.
- Exact operation.
- A SHA-256 fingerprint of the complete descriptor and request input.
- A structural argument summary (sanitized object field names are shown; values are omitted).
- Required permission identifiers and capability-declared consequences.
- The grant and profile that produced the request.
- `single_request` scope and a five-minute expiry.

You resolve an approval, not the model:

```sh
venus-agent permission approvals
venus-agent permission approve 12
venus-agent permission deny 12
venus-agent permission revoke-approval 12
```

An approval stops being usable once it's denied, revoked, expired, or consumed, once its grant is revoked or expired, or once the profile changes. Before policy returns `Allowed`, an approved request gets revalidated against the current descriptor and grant; the approval is then consumed and can't authorize a different request or be reused.

`capability run` shows the action, operation, workspace, effect class, effects, safe argument summary, and required permissions before any policy evaluation. In Controlled or Restricted mode, when approval is required, the command persists a pending request and prints its ID; you review and approve it separately, then rerun the same request with `--approval-id`. The orchestrator revalidates the exact request and the current grant/profile right before invoking the capability. For an allowed request, the final policy check, the approval consumption, and the `started` audit event are committed together before execution, so a grant or profile change slipped in between those checks can't authorize a stale request. Open adds no approval prompt; it still displays the proposed action and only proceeds when the process and the OS permit it. Model-proposed tool calls aren't converted to capability requests yet.

## Audit records and data handling

Inspect metadata-only events with:

```sh
venus-agent permission audit --limit 50
```

Audit events distinguish `proposed`, `denied`, `approved`, `started`, `completed`, `failed`, and `cancelled`, and each one carries the profile, the decision and reason, the capability and version, the operation, the approval reference, the safe argument summary, a timestamp, and a bounded error classification. Invalid lifecycle transitions are rejected. The workspace-summary executor emits start and terminal events; other capability execution isn't supported here yet.

Raw input values, prompts, and model responses never land in capability approvals or audit rows. The SHA-256 request fingerprint is persisted so exact-request matching can be enforced; it isn't an authorization secret, and someone who can read the local database could use it to guess low-entropy inputs, so be careful with sensitive capability arguments. The safe summary shows sanitized field names and a sanitized workspace basename, not the full path or any values.

Grant, approval, profile, and audit data is all local SQLite state. Older schema versions are migrated to version 3; old databases get the Open profile and empty capability tables without losing existing workspace, task, or provider metadata. The provider TOML format doesn't change. Audit history has no retention or pruning setting right now and sticks around until you remove the local database records. It is not tamper-proof against the machine's owner.

## Security boundaries and remaining work

A few limits worth being upfront about, and what's still left to build:

- Application-level profiles are policy, not a security boundary against someone who controls the machine or can modify Venus.
- Venus never elevates OS privileges. Future executors have to run with the real process permissions and handle OS denials explicitly.
- Open stays free of Venus approval gates, and it must never be presented as protected isolation.
- Controlled and Restricted currently gate capability/version/operation/permission IDs, not fine-grained resource boundaries. There are no network, process, or OS isolation policies.
- Authorization is rechecked right before the current read-only executor runs. There's no general in-flight revocation yet for future long-running capabilities.
- Registry descriptors and permission requirements don't prove that an implementation behaves the way it declares. There's no plugin authenticity, code identity, dependency resolver, or process isolation.
- Future capabilities have to treat inputs and external content as untrusted, validate semantics on top of shape, avoid leaking secrets through errors or logs, and never treat model output or a prior approval as your authorization.

What's next: connect model-proposed tool calls to explicit, user-reviewed requests, but only after defining turn limits, cancellation behavior, argument presentation, and safe handling of untrusted model output. And no general file mutation or shell execution until revocation, isolation, audit retention, and OS-boundary behavior are tested.
