# Contributing to agent-ws

Thanks for helping. Bug reports, fixes, docs and new platforms are all
welcome. This page explains how to work on the code.

## Setup

You need macOS on APFS, Git 2.31+ and Rust 1.88+ with `rustfmt` and `clippy`:

```sh
rustup component add rustfmt clippy
git clone https://github.com/mendoanjoe/agent-ws
cd agent-ws
cargo build
cargo test
```

Read [docs/how-it-works.md](docs/how-it-works.md) first; it explains the
records, the pool and the safety rules the code relies on.

## Before you open a pull request

```sh
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
```

All three must pass. Run `cargo test` a few times when you touch anything
concurrent; flaky tests are bugs.

## Tests

- `tests/cli.rs` runs the real binary against throwaway repositories in a
  temp folder, with `AGENT_WS_HOME` pointing into that folder. Follow the same
  pattern: never point a test, or a manual experiment, at a real project.
- Every bug fix starts with a test that fails without the fix.
- Test what a user sees (files in the workspace, branches, command output and
  `--json` fields), not private functions.

## Design rules

These keep agent-ws safe to run unattended. Changes that break them need a
very good reason in the PR description.

1. **The source checkout is read-only.** No command may change files, the
   index or branches in your checkout, except `land` creating a new branch.
2. **Records are not trusted.** State files are plain JSON anyone can edit.
   Check `Repo::owns` before using a record's path; every removal goes
   through `Repo::discard`.
3. **Parallel-safe by construction.** Claims use exclusive create or atomic
   rename; anything done in the background must be safe to race with spawn,
   land, drop, pool fill and gc.
4. **Crash-safe.** Assume `kill -9` at any line. Work in progress records its
   pid and start time so `gc` can clean up after it.
5. **Library first.** Logic goes in the library (`src/lib.rs` and modules);
   `src/main.rs` only parses arguments and prints.

## Style

- Match the surrounding code: small functions, comments that explain why,
  not what.
- Errors are `anyhow` with a message a user can act on.
- No new dependency without a reason in the PR.

## Commits and pull requests

- Conventional prefixes: `feat:`, `fix:`, `docs:`, `test:`, `refactor:`,
  `style:`, `chore:`.
- One topic per PR. Describe what changed, why, and how you tested it.
- Update the README or `docs/how-it-works.md` when behaviour changes.
- If you change performance, include `bench/bench.sh` output before and after.

## Ideas that need help

- Linux support with reflinks (Btrfs, XFS) and a fallback where they are
  missing.
- Smarter pool freshness for ignored files beyond the stamp files.

## Reporting bugs

Open an issue with your macOS version, `agent-ws --version`, the commands you
ran and their output (`--json` helps). For a security problem, please use
GitHub's private vulnerability reporting instead of a public issue.

By contributing you agree that your contributions are licensed under the MIT
License.
