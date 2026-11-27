# Contributing

Contributions are welcome. Keep changes focused, treat the current Rust application as the single source of truth, and don't describe planned behavior as if it already works.

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

Install a stable Rust toolchain with `rustfmt` and `clippy`. Git isn't required to build, but workspace inspection uses it.

Before you open a pull request, run these:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --locked
cargo build --locked
```

A few things I care about when reviewing. Add tests for behavior changes and for the failure cases, not just the happy path. Keep user data and credentials out of fixtures and logs. The code is laid out so each concern has a home: capability descriptors in `src/capability.rs`, bounded orchestration in `src/orchestrator.rs`, profile/grant/approval policy in `src/permissions.rs`, and persistence in `src/database.rs`; keep provider code separate from all of that. Validate untrusted inputs at the boundaries, don't persist raw capability arguments, and recheck authorization right before execution. Any mutating capability has to declare `workspace.write` and require both a matching grant and a fresh exact-request approval in every profile. There's no third-party loader, so don't write as if one exists, and don't imply that a grant installs a capability or that application policy gives you OS isolation.

## Pull requests

- Explain the problem and what behavior changed.
- List the verification commands you ran and what they printed.
- Call out platform limitations and security implications.
- Leave out unrelated formatting churn and generated files.
- Don't commit credentials, tokens, private data, or local application state.

For the current scope and the prioritized gaps, see [docs/architecture.md](docs/architecture.md). For the exact capability contract and profile semantics, see [docs/capabilities-and-permissions.md](docs/capabilities-and-permissions.md).
