# codex-imagegen

An MCP server that lets Claude Code generate images through your local Codex CLI:

1. Claude sends a prompt and gets the image back, as a preview and a file path.
2. It can continue the same Codex session to refine the image, and each refinement returns a new
   version.

Windows only. It ships as a single self-contained executable.

**Status: pre-release.** Generating, refining, resuming a session after a restart, cancelling, a dedicated
Codex home and cleanup have all been checked against the real Codex CLI (0.156 and 0.157). There are no
tagged releases yet. [`docs/design.md`](docs/design.md) describes how it works and why.

## Requirements

- Windows 10 or 11, x64.
- The Codex CLI, signed in with a ChatGPT plan that includes image generation. Images are billed to that
  plan. The free plan is refused, and so is an API-key login: no OpenAI API key is ever used.
- Claude Code.

## Install

1. **Install the Codex CLI and sign it in**, if you have not already. With winget:

   ```powershell
   winget install OpenAI.Codex
   ```

   or, if you have Node.js, `npm install -g @openai/codex`. Then, in a new terminal:

   ```powershell
   codex login
   ```

2. **Get `codex-imagegen.exe`** from the repository's latest GitHub release, where its SHA-256 is
   published next to it. Between releases, every CI run leaves a `codex-imagegen-<commit>` artifact
   (the Actions tab), and you can also build it yourself (see [Development](#development)). Put it in
   a folder outside any repository, for example `C:\tools\`. Claude Code keeps the exe open while it
   runs, so it should not live where a build overwrites it.

3. **Check it from a terminal.** This is free: nothing is generated.

   ```powershell
   C:\tools\codex-imagegen.exe --doctor
   ```

   It exits 0 and reports `image generation: available` when Codex is ready. Otherwise it prints what
   is wrong and how to fix it.

4. **Register it with Claude Code**, for every project (user scope):

   ```powershell
   claude mcp add --scope user codex-imagegen -- C:\tools\codex-imagegen.exe
   ```

   Options go after the exe (see [Options](#options)). Start a new Claude Code session to pick it up.

**Updating.** Close the Claude Code sessions that use it (each one holds the exe open), then copy the new
exe over the old one. Changed options also need a restart of the MCP server, and so does switching the
Codex login to another account: a running Codex keeps the login it started with. After a failed sign-in
check, the next call starts a fresh Codex and picks up the new login by itself.

## Check that it works in Claude Code

Run these in a fresh Claude Code session. The first two are free; the rest spend image quota as noted.

- [ ] **Connected.** In the terminal, `/mcp` lists `codex-imagegen` as connected.
- [ ] **Status.** Ask *"Check codex-imagegen's status."* The report shows `image generation: available`,
      your plan, the Codex version and usage.
- [ ] **Generate** (1 image). Ask *"Use codex-imagegen to generate an image: a cosy reading nook by a
      rain-streaked window at night, soft gouache style."* After about 40 s Claude shows you the image
      and names the file, `generated-images\<session>-v1.png` in the project.
- [ ] **Refine** (1 image). Ask *"Make it daytime, keep everything else."* Claude continues the same
      session and gets `<session>-v2.png`.
- [ ] **Cancel** (optional, 1 image cut short). Ask for another image and press Esc while it is still
      generating in the foreground, before any image comes back. No file appears, and a status check a
      few seconds later shows `running turns: none`. (A call Claude Code has moved to the background is
      stopped with its task controls instead.)

Where the image appears depends on the client. The terminal shows a progress line and the preview in the
tool result. The desktop app shows no progress line and puts the preview in the expanded tool row, so the
result also asks Claude to show you the image itself.

## Using it

Ask in plain language; Claude picks the tool. Three tools are offered:

| Tool | What it does | Arguments |
| --- | --- | --- |
| `codex_imagegen_generate` | Starts a new session and makes an image | `prompt`; optional `session` name, `output_dir`, up to 5 `reference_images` |
| `codex_imagegen_refine` | Edits the session's latest image | `session`, `feedback`; optional `output_dir`, up to 4 more `reference_images` |
| `codex_imagegen_status` | Reports the setup, usage, running turns and this project's sessions | none (free) |

Reference images are files: Claude passes their paths, and each must be a PNG, JPEG or WebP. In the
desktop app you can also paste an image into the chat and ask for a variation, an edit or a remaster of
it, with no file saved first: the app keeps the pasted image as a file and gives Claude its path.

The prompt and the feedback reach Codex's image tool verbatim. A generation takes about 40 s. Claude Code
moves a call that runs past 120 s to the background, and the server stops a call after
`--timeout-seconds` (300 by default).

When a call fails, the result names a code and the fix. A problem Claude can correct itself, such as a bad
argument or a busy session, gets a short correction. Anything else, such as a missing sign-in, a quota
limit or a Codex failure, tells Claude to pass the fix on to you and stop. Neither suggests making an image
some other way.

## Options

Options are arguments on the MCP registration, after the exe. There is no config file.

| Option | Default | What it does |
| --- | --- | --- |
| `--codex-bin <path>` | PATH, then `%LOCALAPPDATA%\Programs\OpenAI\Codex\bin\codex.exe` | The Codex CLI to run |
| `--codex-home <dir>` | your own Codex home | Run Codex with a dedicated home (see below) |
| `--output-dir <dir>` | `generated-images\` in the project | Where new sessions save their images |
| `--timeout-seconds <n>` | 300 | Limit for one generate or refine, 60 to 86400 |
| `--max-concurrent <n>` | 4 | Image turns running at once, across sessions |
| `--session-ttl-days <n>` | 7 | Expire sessions idle this long; 0 disables expiry |
| `--model <id>`, `--effort <level>` | `gpt-6-astra`, `low` | The Codex agent that relays the prompt |
| `--state-dir <dir>` | `%USERPROFILE%\.codex-imagegen\<project>-<hash>` | Where session records are kept |

`C:\tools\codex-imagegen.exe --help` lists them all, with `--doctor` and `--cleanup`. For example:

```powershell
claude mcp add --scope user codex-imagegen -- C:\tools\codex-imagegen.exe --timeout-seconds 600
```

### A dedicated Codex home

By default the server runs Codex with your own Codex home (`%USERPROFILE%\.codex`). The server switches
off your MCP servers, plugins, skills, the shell and web search there, but Codex still loads your
`AGENTS.md` into every image turn, and nothing short of another home stops that. `--codex-home` gives Codex a home of its own instead. Set it up once: create
the folder, then sign it in (PowerShell):

```powershell
New-Item -ItemType Directory -Force "$env:USERPROFILE\.codex-imagegen-home"
```

```powershell
$env:CODEX_HOME = "$env:USERPROFILE\.codex-imagegen-home"; codex login --device-auth; Remove-Item Env:CODEX_HOME
```

The last step removes the variable again, so a `claude` started later from that window does not inherit
it. Then register the server with `--codex-home C:\Users\<you>\.codex-imagegen-home`. Do not end a quoted
path with a backslash: `"C:\x\"` swallows the closing quote.

## Output files

Each image is saved as a full-resolution PNG, `<session>-v<N>.png`. A new session saves into the call's
`output_dir`, else the server's `--output-dir`, else `generated-images\` under the project Claude Code
has open (if Claude Code does not name the project, the `images` folder in the server's state directory).
A refine saves into its session's folder unless the call names another, so changing `--output-dir` later
does not move existing sessions. Nothing is ever overwritten: a taken name moves on to the next version
number.

That folder is scratch space, not a place to keep work: its files expire with their session after
`--session-ttl-days` idle (7 by default). Move or copy the images worth keeping into the project proper,
and keep the folder out of version control by adding this line to the project's `.gitignore`:

```
generated-images/
```

**Cleanup.** When a server first starts Codex, it removes the expired sessions of the project it serves:
the published files, Codex's copies of the images and the Codex session itself. To sweep every project
from a terminal:

```powershell
C:\tools\codex-imagegen.exe --cleanup
```

This covers every project under the state base (`%USERPROFILE%\.codex-imagegen`, or
`%CODEX_IMAGEGEN_HOME%`), with the default age of 7 days, not the options the MCP registration uses: pass
`--older-than-days <n>` for another age (0 removes every session not in use), and `--state-dir` if you
registered one outside the state base. A published file is deleted only while it is still the file that
was published, unchanged; Codex's copies are deleted only from the session's own folder in the Codex home;
read-only files are kept. Sessions in use by a running server are skipped until next time.

## Troubleshooting

- **Start with `--doctor`** in a terminal, or ask Claude for codex-imagegen's status. Both check the
  Codex CLI, the login, the plan, image capability and the model, for free.
- **"Codex version ... outside the tested range"** is a warning only; generation still runs.
- **A session cannot be refined** after it expired, was cleaned up, or was made with another Codex home.
  Start a new one, passing the old image as a `reference_images` entry.
- **Another Claude window has the session open.** A session can be refined by one server at a time. The
  error says so; retry once the other window's call has finished.
- **After changing options**, restart the MCP server: start a new Claude Code session, or reconnect it
  with `/mcp` in the terminal.

## Development

Building needs Rust through [rustup](https://rustup.rs) with the MSVC toolchain (Rust 1.87 or later) and
the Visual Studio C++ build tools with a Windows SDK, which provide the linker. `rust-toolchain.toml`
selects the stable toolchain, the target, rustfmt and clippy.

```powershell
.\build.ps1
```

This runs the formatting check, clippy, the unit tests and a release build, and stages
`dist\codex-imagegen.exe`. The unit tests use a scripted fake Codex; nothing there spends quota.

`.\smoke.ps1` checks the staged binary against your real Codex CLI. On its own it runs only free steps.
The paid modes are billed to your ChatGPT plan:

| Command | Cost | Checks |
| --- | --- | --- |
| `.\smoke.ps1 -SpendQuota` | about 3 images | generate, refine, refine after a restart, status, cleanup, Codex's tool surface |
| `... -Concurrent` | 2 more | two servers generating at once |
| `... -Interrupt` | 1 image plus 1 cut short, instead of the refines | a refine cancelled during its image call |
| `... -CodexHome <dir>` | none extra | every server runs with `--codex-home <dir>` |

`.\smoke.ps1 -CheckTrace <folder>` re-reads the Codex trace a paid run left behind, and spends nothing.
[`AGENTS.md`](AGENTS.md) has the rules for working on the code.

**Releasing.** Set the new version in `Cargo.toml` (run `cargo build` to update `Cargo.lock`) and merge
that through a pull request. Then tag the merged commit on `main` and push the tag:

```powershell
git tag v0.2.0
```

```powershell
git push origin v0.2.0
```

The Release workflow runs the full CI checks on that commit and publishes a GitHub release holding the
exe CI built and its SHA-256. It refuses a tag that is not on `main`, and one that does not match the
version the exe reports. A tag with a suffix, such as `v0.2.0-rc.1`, is published as a pre-release.

## Acknowledgements

This software is based in part on the work of the Independent JPEG Group. The JPEG encoder that builds
the previews includes code derived from the IJG's.
