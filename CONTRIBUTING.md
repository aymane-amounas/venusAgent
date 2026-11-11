# Contributing

Contributions are welcome. Keep changes focused, preserve the current Rust application as the single source of truth, and do not describe planned behavior as implemented.

## Contributor rights and licensing

**Submitting a contribution means you agree to these terms.** You confirm that
you created the contribution or have the necessary rights to submit it. You
retain ownership and grant Aymane Amounas a perpetual, worldwide,
non-exclusive, royalty-free, irrevocable license to use, reproduce, modify,
prepare derivative works from, distribute, and sublicense the contribution as
part of Venus Agent under its project license, including authorizing recipients
to exercise the rights granted by that license.

You expressly authorize Aymane Amounas to offer and grant separate commercial
licenses covering your contribution as part of Venus Agent, without needing
further consent from you. You will not receive compensation for such licensing
unless you and Aymane Amounas separately agree in writing. If you do not agree
to this commercial relicensing grant, do not submit the contribution.

This grant does not change the license for third-party material, which must be
identified and remain under its own terms.

Do not submit material you cannot license on these terms, including code or
other work owned by an employer or another party, unless you have their
permission. These terms apply only to contributions submitted after this policy
is published. They do not retroactively grant rights over earlier
contributions or change the terms of earlier releases.

## Development setup

Install a stable Rust toolchain with `rustfmt` and `clippy`. Git is optional for building, but is used by workspace inspection.

Before opening a pull request, run:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --locked
cargo build --locked
```

Add tests for behavior changes and failure cases. Keep user data and credentials out of fixtures and logs. Capability descriptors live in `src/capability.rs`, bounded orchestration in `src/orchestrator.rs`, profile/grant/approval policy in `src/permissions.rs`, and persistence in `src/database.rs`; keep provider code separate. Validate untrusted inputs at boundaries, do not persist raw capability arguments, and recheck authorization immediately before execution. Mutating capabilities must declare `workspace.write` and require both a matching grant and fresh exact-request approval in every profile. No third-party loader exists. Do not imply that a grant installs a capability or that application policy provides OS isolation.

## Pull requests

- Explain the problem and the behavior change.
- List the verification commands and their results.
- Call out platform limitations and security implications.
- Do not include unrelated formatting or generated files.
- Do not commit credentials, tokens, private data, or local application state.

See [docs/architecture.md](docs/architecture.md) for the current scope and prioritized gaps.
See [docs/capabilities-and-permissions.md](docs/capabilities-and-permissions.md) for the contract and exact profile semantics.
