# Contributing to StellarPOS Contracts

Thanks for helping. This guide covers how to get set up and what a good
pull request looks like here.

## Setup

```bash
git clone https://github.com/<you>/contracts.git
cd contracts
make test
```

You need Rust stable (the toolchain file installs the `wasm32v1-none` target)
and the [Stellar CLI](https://developers.stellar.org/docs/tools/cli) for wasm
builds and deploys.

## Already have a fork?

Every fork of this repository is listed in [CONTRIBUTORS.md](CONTRIBUTORS.md#forks).
Most were created before the current code landed and are many commits behind
`main`. Sync yours before you start, or you will be working on code that no
longer exists:

```bash
# GitHub CLI
gh repo sync <you>/<your-fork> --source StellarPOS-App/contracts --branch main

# or plain git, from your clone
git remote add upstream https://github.com/StellarPOS-App/contracts.git
git fetch upstream
git checkout main && git merge --ff-only upstream/main && git push
```

You can also click **Sync fork** on your fork's GitHub page. Once your first
pull request is merged, you move from the Forks list to the contributors table.

## Workflow

1. **Open or claim an issue first** for anything beyond a typo, so work is
   not duplicated and the design can be agreed early.
2. Branch from `main`: `feat/<short-name>`, `fix/<short-name>`,
   `docs/<short-name>`.
3. Make focused commits using [Conventional Commits](https://www.conventionalcommits.org/):
   `feat(treasury): ...`, `fix(escrow-service): ...`, `test(loyalty-program): ...`.
4. Run `make check` (rustfmt, clippy `-D warnings`, all tests). CI runs the
   same thing.
5. Open a pull request that fills in the template and links the issue with
   `Closes #N`.

## Contract guidelines

- **Return typed errors** from the contract's `#[contracterror]` enum. Do not
  `panic!` or `unwrap()` on anything a caller controls. Append new error
  variants with new numbers and never renumber existing ones: clients depend
  on the codes.
- **Authorize first.** Call `require_auth()` on the right party before
  touching storage or tokens, and say in the doc comment who must sign.
- **Emit an event** (`#[contractevent]`) for every state change.
- **Use checked math** through `shared::bps_of` / `split_fee` or
  `checked_*`. Release builds keep `overflow-checks = true` as a backstop,
  not as the plan.
- **Extend TTLs** with `shared::extend_persistent` whenever you read or write
  a long-lived record.
- **Test the unhappy paths.** Every new error variant needs a test that
  triggers it. Tests go in `src/test.rs`, or under `tests/` for multi-step
  scenarios.
- **Watch wasm size.** Run `make sizes`; the network limit is 64 KB.

## Reviews

A maintainer reviews every pull request. Expect questions about authorization
and failure modes; that is the job, not a judgment of you. Changes to fund
movement or authorization need two approvals.

## Code of conduct

By participating you agree to the [Code of Conduct](CODE_OF_CONDUCT.md).
