# Venus CLI demonstration

A real, unedited session recorded with `venus-agent 0.1.0` on macOS (Apple Silicon) on 2026-11-11. It ran in a fresh demo workspace with an isolated state directory and no model provider configured. Only the workspace path was shortened to `~/demo`.

It shows the full lifecycle of one write:

1. A read-only search runs immediately (Open profile).
2. A file replacement is **denied**: no `workspace.write` grant exists.
3. The user creates a scoped grant. The same request now **requires approval** for its exact arguments.
4. The user approves; the rerun with `--approval-id` executes.
5. Every step is recorded in the SQLite audit log (metadata only, no file content).

`$INPUT` is:

```json
{"path":"src/main.rs","expected_sha256":"<sha256 of the current file>","old_text":"    // TODO: handle config errors","new_text":"    // Config errors are handled in load_config()"}
```

`expected_sha256` makes the replacement fail if the file changed after it was reviewed.

## Session

```console
$ venus-agent init
$ venus-agent capability run workspace.search search --input '{"query":"TODO","case_sensitive":false}'
Proposed action: Search workspace text (workspace.search)
Operation: search
Version: 1.0.0
Workspace: "~/demo"
Effect class: ReadOnly
Effects:
  - Searches visible UTF-8 file content under the selected workspace.
  - Honors hidden-file, ignore, and generated-directory exclusions.
  - Limits each file to 256 KiB, total scanned data to 16 MiB, and results to 50 matches.
Arguments: Object with 2 field(s): case_sensitive, query; other values omitted
Permission requirements: workspace.read (Search visible text files under the selected workspace.)
Completed capability request #1:
{
  "files_scanned": 1,
  "matches": [
    {
      "line": "    // TODO: handle config errors",
      "line_number": 2,
      "path": "src/main.rs"
    }
  ],
  "truncated": false
}

$ venus-agent capability run workspace.file-replace replace --input "$INPUT"
Proposed action: Replace exact text in workspace file (workspace.file-replace)
Operation: replace
Version: 1.0.0
Workspace: "~/demo"
Effect class: Mutating
Effects:
  - Replaces exactly one occurrence of the proposed single-line text in a visible UTF-8 workspace file.
  - Rejects stale file content, hidden/ignored/generated files, symbolic links, and paths outside the workspace.
  - Requires a workspace.write grant and a fresh exact-request approval in every permission profile.
Arguments: Object with 4 field(s): expected_sha256, new_text, old_text, path; other values omitted
Proposed replacement:
-     // TODO: handle config errors
+     // Config errors are handled in load_config()
Permission requirements: workspace.write (Replace a single exact text occurrence in a workspace file.)
Error: Capability request denied: No active grant covers this capability version, operation, and required permissions

$ venus-agent permission grant workspace.file-replace 1.0.0 replace --permission workspace.write
Created grant #1.
This grant configures Venus policy; it does not install a capability.

$ venus-agent capability run workspace.file-replace replace --input "$INPUT"
Proposed action: Replace exact text in workspace file (workspace.file-replace)
Operation: replace
Version: 1.0.0
Workspace: "~/demo"
Effect class: Mutating
Effects:
  - Replaces exactly one occurrence of the proposed single-line text in a visible UTF-8 workspace file.
  - Rejects stale file content, hidden/ignored/generated files, symbolic links, and paths outside the workspace.
  - Requires a workspace.write grant and a fresh exact-request approval in every permission profile.
Arguments: Object with 4 field(s): expected_sha256, new_text, old_text, path; other values omitted
Proposed replacement:
-     // TODO: handle config errors
+     // Config errors are handled in load_config()
Permission requirements: workspace.write (Replace a single exact text occurrence in a workspace file.)
Approval required for request #6: A request-specific user approval is required
Review with `venus-agent permission approvals`, approve with `venus-agent permission approve 1`, then rerun the exact capability request with `--approval-id 1`.
$ venus-agent permission approve 1
Approved request #6 for workspace.file-replace operation replace; the approval is single-request and no operation was executed.

$ venus-agent capability run workspace.file-replace replace --input "$INPUT" --approval-id 1
Proposed action: Replace exact text in workspace file (workspace.file-replace)
Operation: replace
Version: 1.0.0
Workspace: "~/demo"
Effect class: Mutating
Effects:
  - Replaces exactly one occurrence of the proposed single-line text in a visible UTF-8 workspace file.
  - Rejects stale file content, hidden/ignored/generated files, symbolic links, and paths outside the workspace.
  - Requires a workspace.write grant and a fresh exact-request approval in every permission profile.
Arguments: Object with 4 field(s): expected_sha256, new_text, old_text, path; other values omitted
Proposed replacement:
-     // TODO: handle config errors
+     // Config errors are handled in load_config()
Permission requirements: workspace.write (Replace a single exact text occurrence in a workspace file.)
Completed capability request #6:
{
  "path": "src/main.rs",
  "sha256": "8b43b323c42ffd2fd2c974788e7a408e0e759724671ce5100ea67a92a93f5f2b",
  "size_bytes": 87
}

$ cat src/main.rs
fn main() {
    // Config errors are handled in load_config()
    println!("hello");
}

$ venus-agent permission audit
event=9 request=6 workspace.file-replace@1.0.0 operation=replace profile=open decision=allowed reason=The active grant and matching request-specific user approval authorize this request status=completed approval=Some(1) error=None args=Object with 4 field(s): expected_sha256, new_text, old_text, path; other values omitted
event=8 request=6 workspace.file-replace@1.0.0 operation=replace profile=open decision=allowed reason=The active grant and matching request-specific user approval authorize this request status=started approval=Some(1) error=None args=Object with 4 field(s): expected_sha256, new_text, old_text, path; other values omitted
event=7 request=6 workspace.file-replace@1.0.0 operation=replace profile=open decision=approved reason=User approved this exact capability request status=approved approval=Some(1) error=None args=Object with 4 field(s): expected_sha256, new_text, old_text, path; other values omitted
event=6 request=6 workspace.file-replace@1.0.0 operation=replace profile=open decision=approval_required reason=A request-specific user approval is required status=proposed approval=Some(1) error=None args=Object with 4 field(s): expected_sha256, new_text, old_text, path; other values omitted
event=5 request=4 workspace.file-replace@1.0.0 operation=replace profile=open decision=denied reason=No active grant covers this capability version, operation, and required permissions status=denied approval=None error=Some("permission_denied") args=Object with 4 field(s): expected_sha256, new_text, old_text, path; other values omitted
event=4 request=4 workspace.file-replace@1.0.0 operation=replace profile=open decision=denied reason=No active grant covers this capability version, operation, and required permissions status=proposed approval=None error=Some("permission_denied") args=Object with 4 field(s): expected_sha256, new_text, old_text, path; other values omitted
event=3 request=1 workspace.search@1.0.0 operation=search profile=open decision=allowed reason=Open profile adds no Venus permission or approval gate; operating-system permissions remain authoritative status=completed approval=None error=None args=Object with 2 field(s): case_sensitive, query; other values omitted
event=2 request=1 workspace.search@1.0.0 operation=search profile=open decision=allowed reason=Open profile adds no Venus permission or approval gate; operating-system permissions remain authoritative status=started approval=None error=None args=Object with 2 field(s): case_sensitive, query; other values omitted
event=1 request=1 workspace.search@1.0.0 operation=search profile=open decision=allowed reason=Open profile adds no Venus permission or approval gate; operating-system permissions remain authoritative status=proposed approval=None error=None args=Object with 2 field(s): case_sensitive, query; other values omitted
```

Reproduce it with any workspace: `venus-agent init`, then the commands above.
