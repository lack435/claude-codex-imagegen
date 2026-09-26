# AGENTS.md

Instructions for coding agents working in this repository. `CLAUDE.md` imports this file, so it
applies to Claude Code and Codex alike.

## What this project is

`codex-imagegen` is a Windows-only MCP server that lets Claude Code generate and refine images
through a local Codex CLI. It drives `codex app-server`, collects the images Codex's built-in image
tool produces, and hands them back to Claude as a preview plus a file path.

It is written in Rust with the MSVC toolchain and ships as one self-contained binary. Keep the
dependencies to the set named in the design: `serde`, `serde_json`, `png` and `jpeg-encoder`. A small
self-contained binary is a feature, so do not add crates casually.

[`docs/design.md`](docs/design.md) is the source of truth for behaviour. When a change alters
behaviour, update the design in the same change.

## How much rigor, and where

Be careful where being wrong is expensive, and proportionate everywhere else.

**Rigor belongs where the blast radius is real.** Enumerate edge cases and fail closed for:

- billing and credentials: no API-key mode, API-key variables stripped from the child, a `chatgpt`
  account required;
- the Codex child's posture: read-only sandbox, `approvalPolicy: never`, shell tool and user MCP
  servers off;
- process reaping: the job object;
- anything that writes to or deletes from the caller's output directory or Codex's home. The
  cleanup safety rules in the design are deliberate.

**For a generation itself, the worst case is that it is lost and re-run**: some quota and about a
minute. Avoid that, but do not fortify against every edge case to prevent it. The design's rule is
that an image that completed is never reported as a failure, and that one does deserve care.

**Review ratchets toward rigor, and you are the counterweight.** Every review round asks "what
about this edge case", every honest answer adds machinery, and nothing in the loop argues for less.
When a finding's fix adds machinery:

- ask whether the machinery is proportionate to what it prevents;
- be willing to answer the finding by removing whatever made it reachable.

Cutting scope in response to review is a legitimate resolution. Say so plainly when you do it.

## Before handing work back

```powershell
.\build.ps1
```

This runs `cargo fmt --check`, clippy with `-D warnings`, the unit tests and a release build, then
stages `dist\codex-imagegen.exe`. If a Claude Code session has the server open from `dist\`, the
copy fails, and `build.ps1` reports the process holding the file rather than leaving a stale binary.

- **`cargo test`** is unit tests only. No network, no model calls, and no real Codex. The app-server
  client is tested against a scripted fake child.
- **`smoke.ps1`** is a real end-to-end run against Codex. It spends image quota (about 3 images), so
  run it when a change touches the protocol, spawning or session handling, and tell the user the
  cost before running it. `build.ps1` passing is not a substitute, because it never starts Codex.
- **If OpenAI's service is degraded**, live checks fail for reasons unrelated to the code. Say so,
  rather than "fixing" code to make an outage pass.
- **CI** is Windows-only by design. Do not weaken the failure-contract check once it exists.

## Reviews and merging

Work on a branch, and merge to `main` through a pull request. Before merging, get an independent
review from a different model than the one that wrote the change. Then address each finding, or
answer it explicitly.

## Conventions that are easy to get wrong

- **Never commit `codex-imagegen.exe`.** `dist\` is gitignored. Distributed binaries come from CI,
  never from a workstation: the GitHub release a `vX.Y.Z` tag on `main` publishes
  (`.github/workflows/release.yml`), or the per-commit build artifact between releases.
- **Release build flags live in `build.ps1`.** Its `CARGO_ENCODED_RUSTFLAGS` overrides every
  `rustflags` key in `.cargo/config.toml`, so a flag the release needs, such as the path remapping or
  the static CRT, must go in that list.
- **Pin models by full id** (`gpt-6-astra`). Aliases move.
- **stdout is protocol traffic only.** All diagnostics go to stderr.
- **The Codex child's isolation and posture are security boundaries.** This covers:
  - the spawn switches;
  - the per-thread MCP-off map and its canary;
  - the stripped API-key variables;
  - the read-only sandbox and never-approve policy;
  - the job-object reaping;
  - the cleanup safety rules.

  Each exists for a reason recorded in the design, with its evidence. Do not relax any of them
  without saying plainly which boundary moves.
- **When generation fails, relay the remediation and stop.** Error text returned to Claude must
  never invite it to substitute an image made some other way, and must never claim an image exists
  when none does.
- **Claim only what was verified.** The design tags claims [verified], [assumed] or [decided].
  Keep that discipline in code comments, docs and what you tell the user. A design doc starts with
  a `Status:` line.
- **This repository is self-contained.** Do not reference other projects in code, docs or commit
  messages.
