# codex-imagegen v1 design

Status: approved by the owner, 2026-09-25 (revision 3: adversarial review applied; cleanup added). M0 (repo
scaffold) is in place. M1 is implemented: the MCP layer, `status` and `--doctor`, spawn, handshake and preflight,
and the CI contract check. M2 is in progress: output publishing (`output.rs`), the preview pipeline
(`preview.rs`) and the turn registry (`registry.rs`) exist with their unit tests, but `generate` is not wired to
them yet, so `generate` and `refine` still validate their arguments, run preflight and return `INTERNAL_ERROR`.
V0 is not yet run.

Claims carry one of three tags:

- **[verified]**: observed on this machine (codex-cli 0.156.0, Claude Code 2.1.280, Windows 11), or read in
  source at a named tag.
- **[assumed]**: expected from source or documentation, but not yet exercised. Each has an entry in
  [Verification plan](#verification-plan).
- **[decided]**: a v1 choice. It can be revisited, but it is not an open question.

## Goal

An MCP server that Claude Code uses to have a local Codex CLI generate images:

- Claude sends a prompt and gets the image back.
- Claude can continue the same Codex session to ask for refinements, and each refinement returns a new image.

The image generation itself happens in Codex's built-in image tool, billed to the user's ChatGPT plan. This
server drives Codex, collects the images, and hands them to Claude.

Out of scope for v1:

- an OpenAI API-key mode
- choosing the image model, size or quality
- several variants per call
- branching from an earlier version
- a cancel tool (see [Progress and cancellation](#progress-and-cancellation))
- metrics
- platforms other than Windows

## What Codex actually provides

These facts constrain the design.

**The image tool.**

- Codex's built-in `image_gen.imagegen` tool takes exactly
  `{prompt, referenced_image_paths?, num_last_images_to_include?}` [verified: binary strings;
  `ext/image-generation` at `rust-v0.156.0`].
- The client always requests `gpt-image-2`, with size and quality set to `auto`. The backend chooses the
  engine and never says which one ran. So neither this server nor Codex can pick the image model, size,
  quality, transparency or image count [verified].
- `gpt-6-astra` is catalogued as `tool_mode: code_mode_only`. The agent reaches the image tool only as the
  nested `tools.image_gen__imagegen({...})`, inside a JavaScript snippet sent to the code-mode `exec` tool.
  That snippet runs in a V8 isolate with no Node, no filesystem and no network. The app-server item stream
  is unaffected [verified: smoke rollout; `bundled_models.json`; `core/src/tools/mod.rs`].

**Output.**

- Each tool call makes one PNG of about 1.5–1.6 MP. Sizes seen so far are 1254×1254, 1312×1199, and aspect
  ratios that follow the reference image.
- The file is 8-bit RGB, 1.4–3.5 MB, and includes a C2PA `caBX` chunk [verified].
- The backend may return a transparent background: the client sends `background: auto` [verified: source].
  No transparent output has been seen yet.
- A turn takes 37–41 s, of which 24–30 s is the image call. The agent spends another 4–6 s writing its
  closing line after the image is done [verified: smoke logs].

**Transport.**

- `codex exec --json` drops image items entirely [verified: source].
- `codex app-server` reports them as typed items [verified: live]:

  ```
  item/started   {item:{type:"imageGeneration", id:"exec-<uuid>", status:"in_progress", …}}      ~370 B
  item/completed {item:{type:"imageGeneration", id, status:"completed"|"failed", revisedPrompt,
                        result:<base64 PNG, or "" when failed>, transparentBackground,
                        failure:null|{type:"usageLimitExceeded",limitId,resetsAt},
                        savedPath?:"<CODEX_HOME>\\generated_images\\<threadId>\\<id>.png"}}       up to ~3.8 MB, one line
  turn/completed {threadId, turn:{id, status:"completed"|"interrupted"|"failed", error}}
  ```

- `app-server` is labelled "[experimental]". See [Codex version pinning](#codex-version-pinning).

**How failures surface.**

- When the image backend errors (a refusal, a 5xx, no data, a usage limit), the tool still emits
  `item/completed` with `status:"failed"`, an empty `result` and no `savedPath`. `failure` is set only for
  the `image_gen` usage limit.
- The error text goes only to the agent model, and the turn usually still ends as `completed`
  [verified: `tool.rs:171-262`].
- Argument and reference-read errors, such as an unreadable reference path, happen before `item/started`.
  They produce no item at all [verified: `tool.rs:147-150, 420-470`].

**Refinement.**

- In the smoke test, Codex passed the previous image's `savedPath` in `referenced_image_paths`, and the edit
  kept the composition.
- `revisedPrompt` equalled the prompt sent, character for character [verified].
- After an `app-server` restart, `thread/resume` continued the thread with full history [verified].
- Each turn made two model requests. The prompt grew about 3.6k input tokens per turn, from 15.7k on the first
  request to 25.2k on the last over three turns, and about 90% of it was cached after the first turn. The three
  turns billed about 34k, 41k and 48k input tokens [verified].

**Quota.**

- Images are metered in their own `image_gen` bucket. The tool reports an item-level failure only for that
  limit, and its window is 1,440 minutes [verified: source and tests].
- `account/rateLimits/read` returned only the `codex` bucket, the agent's weekly allowance. It stayed at 44%
  across three images [verified]. So the `codex` percentage does not predict whether images are available.

**Threads are locked across processes.**

- Loading a thread (`thread/start` or `thread/resume`) takes an exclusive OS lock on
  `<CODEX_HOME>\thread-writer-locks\<id>.lock`. The lock is held until the thread unloads.
- A thread unloads only when no connection is subscribed to it and it has been idle for
  `thread_unload_delay_secs` (60 s by default). The caller of start or resume is subscribed automatically.
- A second process's `thread/resume` fails with `-32600 "thread <id> already has an active writer"`.
  After the holder unsubscribes, `thread/closed` arrives once the unload delay has passed (5.0 s with
  `thread_unload_delay_secs=5`), and the second resume then succeeds [verified: live, V7].
- `turn/start` on a thread whose turn is still running is merged into that turn, not queued
  [verified: source].

**Free calls.** These spend no quota [verified]:

- `initialize`, whose reply includes `codexHome` and `userAgent`
- `account/read`, which returns `{account:{type,email,planType}|null, requiresOpenaiAuth, workspaceRouting}`
- `modelProvider/capabilities/read`, which returns `{imageGeneration:bool,…}` (provider capability only, not a
  login check)
- `account/rateLimits/read`, which returns `{rateLimits:{limitId,primary,secondary,…}, rateLimitsByLimitId,…}`,
  with each window being `{usedPercent, windowDurationMins, resetsAt}`
- `model/list`, `config/read`
- `thread/start` without a turn

**Isolation.**

- `app-server` has no `--ignore-user-config`, and `--profile` is rejected [verified: source and live].
- `-c` overrides and thread-level `config` add or override keys but never delete them [verified].
- The user's `$CODEX_HOME/AGENTS.md` is always loaded. Only a different `CODEX_HOME` avoids it [verified].
- A running `app-server` never re-reads `auth.json`. A login made in a terminal is picked up only by a newly
  spawned child [verified: source].

## Claude Code facts that shape the tool surface

Verified by reading the Claude Code 2.1.280 bundle, and against the official docs.

**Image blocks.**

- Image content blocks reach the model as real images. Accepted types are png, jpeg, gif and webp.
- Images larger than 2000×2000 are downscaled.
- An image over 512,000 raw bytes is re-encoded as lossy JPEG. We therefore send our own preview, below that
  size.

**Structured content.**

- If a result has `structuredContent`, the model gets only the non-text blocks plus that JSON. The server's
  own text blocks are dropped.
- Declaring `outputSchema` makes structuredContent mandatory.
- **[decided]** No outputSchema and no structuredContent. Every fact goes in a text block.

**Error results.** Results with `isError:true` pass only their text. Images are dropped.

**Timeouts and progress.**

- A per-server `timeout` is a hard wall-clock limit. The default is effectively unlimited, about 27.8 h, and
  progress does not extend it.
- A 30-minute idle watchdog is reset by progress.
- Progress messages are shown to the user, cut to 200 characters.
- `progressToken` is the numeric request id.

**Backgrounding.** A running interactive call moves to the background after 120 s, *or as soon as the user
sends a message while it runs*. Then:

- Its result arrives later as a task notification, with images saved to disk. The model sees the image only
  through the path in our text block.
- Esc no longer reaches a backgrounded call. Claude Code's TaskStop (or the task's kill key) aborts the request
  instead, and the SDK then sends `notifications/cancelled` [verified: bundle].

**Output limit.** `MAX_MCP_OUTPUT_TOKENS` defaults to 25,000, and each image is estimated at a flat 1,600
tokens.

**Parallel calls.** MCP tools without `readOnlyHint` run one at a time within a Claude Code session. Parallel
calls come from separate windows or subagents.

**Protocol version.** Claude Code sends `protocolVersion: "2025-11-25"`.

**Image cost.** An image costs `ceil(w/28) × ceil(h/28)` tokens. A preview with a 1024 px long edge costs at
most 1,369 tokens and is not resized on any model tier.

## Tools

Three tools, prefixed `codex_imagegen_` [decided].

**Session names.**

- Names match `[A-Za-z0-9._-]{1,64}` and are compared case-insensitively, because NTFS file names are.
- A new name that differs from an existing one only in case gets `SESSION_EXISTS`.

### `codex_imagegen_generate`

Starts a new session and generates one image.

| arg | type | notes |
| --- | --- | --- |
| `prompt` | string, required | Passed to the image tool verbatim. |
| `session` | string, optional | The new session's name. If omitted, the server picks `img-<yyyyMMdd-HHmmss>-<4 hex>`, and picks again if that collides. If an explicit name exists: `SESSION_EXISTS` (agent-correctable). |
| `reference_images` | string[], optional | Up to 5 paths. Each must open and start with a PNG, JPEG or WebP signature, or the call gets `BAD_REQUEST`. Relative paths resolve like output paths: against `CLAUDE_PROJECT_DIR` when it is set, else the server's working directory. They reach Codex as absolute paths, and become the tool's `referenced_image_paths`. |
| `output_dir` | string, optional | See [Output files](#output-files). |

### `codex_imagegen_refine`

Continues a session. It edits the session's latest image and returns the new version.

| arg | type | notes |
| --- | --- | --- |
| `session` | string, required | Must exist, or `SESSION_NOT_FOUND`. |
| `feedback` | string, required | Passed to the image tool verbatim as the edit prompt. |
| `reference_images` | string[], optional | Up to 4 more images, validated like generate's. The first slot is the edit target. |
| `output_dir` | string, optional | Defaults to the session's recorded output directory. |

### `codex_imagegen_status`

No arguments and no quota. It reports:

- the server version and build
- the Codex binary and version, and whether that version is in the tested range
- whether `app-server` is running
- the account type and plan
- image-generation availability
- whether the pinned model is listed (and whether it is hidden)
- every usage window (see [Usage display](#usage-display))
- running turns, with session and elapsed time
- sessions in this project: name, turns, latest output path, last update

Live reads are bounded to about 10 s each. On timeout, the last value is shown with its age.

If the child exits during a live read, or is gone by the time the report is built, it is not running, whatever
preflight found. The report then shows the app-server not running and image generation unavailable
(`APP_SERVER_FAILED`, with the detail), the child is discarded so the next call starts a fresh one, and
`--doctor` exits 1. A read that fails while the child is still alive (a timeout or an error reply) only makes
the usage stale.

### Success result (generate and refine)

```
content: [
  {type:"image", mimeType:"image/jpeg", data:<preview base64>},
  {type:"text", text:
     "session: fox-watercolor   version: 2\n" +
     "image: C:\\proj\\generated-images\\fox-watercolor-v2.png  (1312x1199, 2.6 MB PNG)\n" +
     "preview above is a 1024px JPEG; the file is the full-resolution original\n" +
     "codex prompt: <revisedPrompt>\n" +
     "codex note: <Codex's closing line, quoted, untrusted>\n" +
     "took 38.1 s; Codex agent usage: weekly 44% (resets 2026-10-01 14:17)\n" +
     "output files are scratch and expire after 7 days idle; move keepers into the project"}
]
```

A result returns success whenever at least one image completed, whatever happened afterwards [decided].
Anything unusual is added as a `warning:` line:

- `revisedPrompt` differs from the text sent;
- the turn was interrupted, timed out or failed after the image finished;
- the copy to `output_dir` failed, so the image line points at `savedPath` instead;
- the preview could not be built, so there is no image block;
- the session record could not be updated;
- another image call in the same turn failed.

## Request flow

```
Claude ── tools/call generate|refine
  1. validate: prompt/feedback non-empty, session name, reference image signatures,
     output_dir resolved + created + probe-written              ── any failure: BAD_REQUEST, nothing spent
  2. registry try_start (busy / concurrency cap / shutdown) and the per-session cross-process lease
  3. ensure the child: spawn in a kill-on-close job → initialize → preflight (once per child)
  4. config/read → build the MCP-off map for this thread
  5. generate: thread/start          refine: pick the edit target (see Refine), then thread/resume
  6. turn/start {threadId, input, model, effort}
         ◀── notifications/progress every ~5 s and on phase changes
     item/started   imageGeneration → phase "generating image"
     item/completed imageGeneration → status "completed": copy → preview → update the session record, at once
                                      status "failed": note it (see Errors), no file, no version
     item/completed agentMessage    → keep the last line
     turn/completed                 → finish
  7. thread/unsubscribe {threadId}   (always: completed, failed, interrupted or timed out)
  8. release the lease; return a result built from the completed images, or an error if there were none
```

**Deadlines.**

- The whole call counts against `--timeout-seconds` (default 300): spawn, preflight, resume, turn, copy and
  preview.
- Each app-server request has its own deadline: about 30 s for the handshake, preflight calls,
  `config/read`, `thread/start` and `turn/start`. `thread/resume` gets whatever remains of the call's budget.
- A missed deadline returns `APP_SERVER_FAILED`, naming the method, or `TIMEOUT` once the overall budget is
  gone.
- The shared child is recycled only when it has no other turns running, or when a cheap liveness call
  (`config/read`) also misses a short deadline.

### Refine

1. **Pick the edit target** before any turn: the first of `last_saved_path` and `last_output_path` that exists
   with the recorded size (they are byte-identical copies). If neither does: `SESSION_NOT_RESUMABLE`, with
   the remediation "start a new session with `generate(reference_images=[<a surviving copy>])`". Nothing is
   spent.
2. **Always call `thread/resume`** with `excludeTurns: true` and the thread parameters, before every
   `turn/start`. On a thread still loaded in this child, that only re-subscribes. Errors:
   - "is closing; retry": retry after 250 ms.
   - "already has an active writer": another process has the thread loaded. It may be another codex-imagegen
     still inside its unload delay. Retry with backoff for up to `thread_unload_delay_secs + 10` s, with the
     phase "waiting for the session to be released". Then return `SESSION_OPEN_ELSEWHERE`, agent-correctable:
     the session is open in another Claude Code window or in the Codex app; close it there, or use a new session.
   - "no rollout found": `SESSION_NOT_RESUMABLE`.
   - Any other error: `APP_SERVER_FAILED` with the detail.
3. **Name the edit target explicitly** in the input. Codex never picks it from memory [decided].

### Input text sent to Codex

Tagged, so the literal prompt stays separate from metadata:

```
<image_prompt>
{prompt}
</image_prompt>
<reference_images>          ← only when given
C:\path\a.png
</reference_images>
```

Refine uses `<edit_request>` in place of `<image_prompt>`, plus `<edit_target>{path}</edit_target>`.

### developerInstructions

Set on `thread/start` and `thread/resume`:

> You are a headless image-generation backend driven by a program, not a person. For each user message, call
> the image generation tool exactly once. Calling it through `exec` is expected; inside `exec`, call only the
> image generation tool. Use the text inside `<image_prompt>` or `<edit_request>` as the tool's `prompt`,
> character for character: do not rewrite, expand, translate or summarise it. Set `referenced_image_paths` to
> the `<edit_target>` path, if present, followed by every `<reference_images>` path in order. Omit it when
> there are none, and never use `num_last_images_to_include`. If the tool returns an error, do not call it
> again; reply with the error text. Do not call shell or file tools, do not create, copy or move files, and do
> not ask questions. After the tool returns, reply with one short line describing the result.

## Codex child process

### Spawn

The child is spawned lazily, on the first tool call that needs it. It is never spawned during `initialize`:
Claude Code's `MCP_TIMEOUT` is 30 s, and a future negotiation mode may start a throwaway probe copy of this
server.

```
codex app-server --listen stdio://
  --enable image_generation
  --disable apps --disable plugins --disable hooks --disable memories --disable multi_agent
  --disable goals --disable shell_tool --disable tool_suggest --disable skill_search
  --disable browser_use --disable computer_use --disable in_app_browser
  -c notify=[] -c skills.bundled.enabled=false -c skills.include_instructions=false
  -c web_search="disabled" -c approvals_reviewer="user" -c windows.sandbox="unelevated"
  -c thread_unload_delay_secs=5
```

**What has been checked.**

- Every switch was accepted and shows up in `config/read` [verified].
- For apps, plugins, bundled skills, notify and MCP servers, the effect also shows in `skills/list`,
  `plugin/list` and `mcpServerStatus/list` [verified].
- For the rest, and for the image tool still being offered through `exec` under these switches, the effect is
  known from source only [assumed: V1].
- `--enable image_generation` guards against a user config that turns the tool off.

**Environment.**

- The child inherits the environment minus `OPENAI_API_KEY`, `CODEX_API_KEY` and `CODEX_ACCESS_TOKEN`
  [decided], so billing cannot silently move off the ChatGPT plan.
- `CODEX_HOME` is set only with `--codex-home`.

**Working directory.** `<state base>\work`, an empty directory outside any repository. No project `AGENTS.md`,
config or trust entry comes into play [decided].

**Job object.** The child runs in a Windows job object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. It is
spawned suspended, assigned to the job, then resumed, so the whole `app-server` tree dies with codex-imagegen,
even if codex-imagegen crashes [decided].

**stderr.** Drained continuously, keeping a bounded tail for failure details.

### Handshake and preflight

```
initialize {clientInfo:{name:"codex-imagegen", title:null, version}, capabilities:{
  experimentalApi:false, requestAttestation:false,
  optOutNotificationMethods:[item/agentMessage/delta, item/reasoning/summaryTextDelta,
    item/reasoning/summaryPartAdded, item/reasoning/textDelta, item/plan/delta, turn/plan/updated,
    turn/diff/updated, thread/tokenUsage/updated, remoteControl/status/changed, skills/changed]}}
→ initialized
```

Preflight runs **once per child instance**, and again after every spawn or respawn:

1. **`account/read`.** A null account fails with `NOT_AUTHENTICATED`. A type other than `chatgpt` also fails
   with `NOT_AUTHENTICATED`, whose remediation says API-key auth is unsupported. `planType: "free"` fails
   with `IMAGEGEN_UNAVAILABLE`.
2. **`modelProvider/capabilities/read`.** `imageGeneration:false` fails with `IMAGEGEN_UNAVAILABLE`.
3. **`model/list`** with `includeHidden: true`, following `nextCursor`. If the pinned model is absent:
   `MODEL_UNAVAILABLE`. If it is present but hidden, `status` notes it, since a hidden model is often being
   retired.
4. **`config/read`**, with `cwd` set to the work directory. The effective config must show every spawn switch
   in effect: `features.image_generation` true, each switched-off feature false, `web_search`, `notify`,
   `skills.*`, `approvals_reviewer` and `windows.sandbox` as set. A legacy alias of a switched-off feature
   (`connectors`, `memory_tool`, `collab`, `codex_hooks`) must not be true. Anything else fails with
   `IMAGEGEN_UNAVAILABLE`, naming the setting. Our switches outrank every layer except legacy managed config
   (`managed_config.toml`) [verified: source, `config/src/config_layer_source.rs`]. So this catches that
   layer, managed requirements, and a legacy alias in the user's config: `memory_tool = true` turned
   `memories` back on despite `--disable memories` [verified: `config/read` on a test home], and `connectors`
   is applied after `apps` [verified: source, `features/src/lib.rs` `apply_map`]. A `config/read` error also
   fails with `IMAGEGEN_UNAVAILABLE`, quoting Codex. A legacy top-level `profile = "..."` key causes one
   ("legacy `profile` config is no longer supported"), while the app-server logs "Invalid configuration;
   using defaults" and keeps serving [verified: live]. Profiles never apply to `app-server`: 0.156.0 passes no
   profile features, and profiles v2 need `--profile` [verified: source, `core/src/config/mod.rs`].
5. **Version.** Taken from the child's `userAgent` and checked against the tested range.

**When preflight fails.** The child is closed at once. It never ran a turn, so nothing is lost. The next call
spawns a fresh one, and that one re-reads `auth.json`. So a retry after `codex login` works without
restarting Claude Code [decided].

**When a turn fails with `AUTH_EXPIRED`.** The child is marked for recycling and closed once no turns are
running on it.

**`account/updated`.** An `authMode` other than `chatgpt` invalidates the cached preflight.

### Thread and turn parameters

- **Before every `thread/start` and `thread/resume`**, call `config/read` (local and free) and pass
  `config:{mcp_servers:{<each name present>:{enabled:false}}}`. The map is rebuilt from that read every time,
  so a server the user adds mid-session never loads. A server that was removed never leaves a stale entry
  behind, which would otherwise fail every later config build [verified: source].
- **If the load fails** with a config error mentioning `mcp_servers`, re-read the config and retry once.
- **`thread/start`:**
  - `model`: `gpt-6-astra`, full id [decided]
  - `cwd`: the work directory
  - `sandbox`: `"read-only"`
  - `approvalPolicy`: `"never"`
  - `approvalsReviewer`: `"user"`
  - `developerInstructions`
  - `config`: the MCP-off map
  - `ephemeral`: false
- **`thread/resume`:** the same, plus `threadId` and `excludeTurns: true`.
- **`turn/start`:** `threadId`, `input:[{type:"text", text, text_elements:[]}]`, `model`, and `effort: "low"`
  [decided]. The agent model only relays the prompt.

### Message handling

**Reading.** One reader thread reads child stdout through `BufReader`, with a 64 MiB cap on line length.

**Writing.** One writer thread owns the child's stdin and writes queued lines in order [decided]. Requests,
notifications and the refusals of server requests only queue a line. A child that stops reading fills the pipe,
and the next write then blocks until the child dies. Only the writer thread waits on it: each request still ends
at its deadline or cancel, the reader keeps routing, and shutdown still reaches the job after its grace. The
job's kill then fails the stuck write ("the pipe has been ended"), which ends the writer thread [verified: unit
test against `PING.EXE`, which never reads its stdin].

**Parsing.** Two stages:

1. A borrowed envelope, in which `id`, `method`, `params`, `result` and `error` are each a `&RawValue`.
2. A typed struct per method, with `imageGeneration.result` as `IgnoredAny`.

Measured: about 1 ms and under 1 KB allocated per 3.8 MB line, against about 1.3 ms and a 3.8 MB copy through
`serde_json::Value` [verified: benchmark]. The saving is memory.

**Routing.**

- Replies are matched by id. They can arrive out of order [verified].
- Notifications are routed by `threadId` to the running turn's handler.
- The reader thread only routes. Copies and previews run on the call's own thread, so one session's 3 MB copy
  never stalls another session's messages.

**Image items.**

- Check `status` before reading anything else.
- Only a `completed` item with a `savedPath` is copied.
- A `completed` item with no `savedPath` is re-parsed with `result: Cow<str>`, and the base64 is decoded
  instead.
- A `failed` item is handled under [Errors](#errors).

**Server-to-client requests.** These include approvals, `requestUserInput`, elicitation and
`chatgptAuthTokens/refresh`. Each is answered at once with JSON-RPC error `-32601` and logged, so a turn can
never hang [decided].

**Canary.** `mcpServer/startupStatus/updated` is deliberately *not* opted out. If it reports on one of our
threads a server that is not in the disabled map, the turn is interrupted and the call fails with
`APP_SERVER_FAILED` ("isolation breach: MCP server <name> started").

**Usage updates.** `account/rateLimits/updated` is merged into the usage cache under its `limitId` (see
[Usage display](#usage-display)).

### Lifecycle

- **Child death.** In-flight turns fail with `APP_SERVER_FAILED`, unless an image already completed (see the
  success rule). The next call respawns the child and re-runs preflight.
- **Our stdin closing.** When our stdin closes and we are given time to exit:
  1. interrupt running turns;
  2. let copies already in progress finish;
  3. end the child's input: the writer thread writes what is still queued, then closes the pipe (an idle child
     exits in 0.05–0.07 s [verified]);
  4. wait up to 5 s in total;
  5. drop the job. This step never waits on the pipe, so a child that stopped reading is killed on time.

  This sequence is best-effort. Claude Code 2.1.280 closes stdin and then kills the server's process tree
  straight away [verified: bundle], so under Claude Code it usually does not run. Nothing depends on it:
  - the job object reaps the Codex tree however we die;
  - session records are written atomically as each image completes;
  - cleanup is safe to retry after a crash.

### Codex version pinning

- The tested range starts as exactly 0.156.x. Outside it, `status` warns and generation still proceeds
  [decided], because a hard refusal would break on every Codex auto-update.
- Parsing is defensive:
  - unknown fields are ignored;
  - unknown item types are skipped;
  - protocol anomalies (for example a reply shape we cannot parse) give `APP_SERVER_FAILED`, naming the Codex
    version.

## Sessions

- **What a session is.** A caller-chosen name mapped to a Codex thread.
- **Where they are stored.** In a per-project store at
  `<state base>\<project-leaf>-<fnv64(lowercased cwd)>\sessions.json`. The state base is
  `%CODEX_IMAGEGEN_HOME%`, or else `%USERPROFILE%\.codex-imagegen` [decided]. It is deliberately not under
  `%LOCALAPPDATA%`, which packaged (MSIX) hosts redirect.
- **Record fields.** `{thread_id, codex_home, model, created, updated, turns, last_saved_path,
  last_output_path, last_output_bytes, output_dir, next_version, outputs:[{version, path, bytes}]}`.
  `outputs` lists every file this server published for the session. [Cleanup](#cleanup) uses it.
- **When the record is written.** When the session's first image completes, not at turn end. It is updated on
  every later completed image, in stream order. A turn that produces no completed image leaves a new name free,
  and leaves an existing record unchanged.
- **How it is written.** Atomically: a temp file, then rename with retry. A `LockFileEx` byte-range lock on a
  sibling lock file serialises access, and the OS releases it if the process dies.
- **Two kinds of exclusion.**
  - The per-session **lease** (a `LockFileEx` lock per name, held for the whole call) stops a same-name
    generate from spending quota twice across processes. It also keeps the record's read, turn and write in
    order.
  - **Cross-process exclusion on the thread itself** comes from Codex's writer lock. We release it by
    unsubscribing after every turn, so a session created in one Claude Code window can be refined from another
    about 5 s later.
  - Within one process, the registry refuses a second call on a busy session with `SESSION_BUSY`, so a
    `turn/start` never merges into a running turn.
- **Concurrency.** Different sessions run concurrently on the same child. The global cap is `--max-concurrent`
  (default 4), beyond which calls get `TOO_MANY_RUNNING` [decided].
- **A different Codex home.** A record whose `codex_home` differs from the child's `codexHome` gets
  `SESSION_NOT_RESUMABLE`.
- **Housekeeping.** Sessions expire. See [Cleanup](#cleanup).

## Output files

**Directory.** The first of these that applies:

1. the call's `output_dir`;
2. for refine, the session's recorded `output_dir`;
3. the server's `--output-dir`;
4. `%CLAUDE_PROJECT_DIR%\generated-images`, when Claude Code sets `CLAUDE_PROJECT_DIR` [decided];
5. otherwise `<per-project state dir>\images`.

Relative paths, whether from the argument or the flag, resolve against `CLAUDE_PROJECT_DIR` when it is set,
and otherwise against the server's working directory. So one user-scope registration gives every project its
own folder.

`generated-images/` is **scratch space** [decided]. Its files expire with their session (see
[Cleanup](#cleanup)). Claude moves or copies anything worth keeping into the project proper. The tool
descriptions and `initialize` instructions say so, and so does every success result:
`output files are scratch and expire after N days idle; move keepers into the project`. The README suggests
adding `generated-images/` to `.gitignore`.

**Pre-check.** Before anything is spent, the directory is created with `create_dir_all`, and a temp file is
created in it and deleted. Any failure is `BAD_REQUEST`, naming the path.

**File names.**

- Files are named `<session>-v<N>.png`.
- The copy goes to a temp file in the destination directory, `.<session>-v<N>.<pid>-<seq>.tmp`, and is then
  published with `MoveFileExW` *without* `MOVEFILE_REPLACE_EXISTING`.
- On `ERROR_ALREADY_EXISTS` or `ERROR_FILE_EXISTS`, N is bumped and the publish retried, and `next_version`
  records the N actually used plus one. A folder in the way counts as taken [verified: unit test].
- The temp file is flushed to disk before the rename, so the published name never shows a partial file. A
  rename refused with a sharing or access error, typically an antivirus scanner holding the fresh file, is
  retried for about 0.8 s before it counts as a failed copy.
- Nothing is ever overwritten, even when several projects or processes share a folder [decided].

**Copy.** A byte-for-byte copy of `savedPath`, so the C2PA chunk survives. It is never re-encoded. If the copy
fails despite the pre-check, the result is still a success: its image line points at `savedPath`, with a
`warning:` line, and the record is still updated.

**Preview.** Built from the image bytes:

1. Decode with `png`, normalised to 8 bits per sample.
2. Downscale with an exact area average to a long edge of at most 1024 px. Never upscale.
3. Encode as JPEG, quality 85, with `jpeg-encoder` (4:2:0, standard Huffman tables).
4. Base64.

Measured: 89–317 KB, about 50 ms [verified: benchmark].

If the JPEG is over 512,000 bytes, the quality steps down (75, 65, 50, 35) until it fits. Real images fit at 85
[verified: benchmark]; only noise-like content needs the steps (a 1024 px noise image fits at 50 [verified: unit
test]). If nothing fits, there is no image block, only the warning line.

If the decoded image has transparency (any pixel less than opaque; an alpha channel that is opaque everywhere
does not count), the preview is a PNG, downscaled with colour premultiplied by alpha so transparent pixels
cannot darken the edges. If that PNG is over 500 KB, it is flattened onto white and sent as JPEG instead, and
the text says so.

**Huffman trap.** `jpeg-encoder`'s `set_optimized_huffman_tables(true)` produces non-interleaved 4:2:0 scans.
These decoded as garbage in `zune-jpeg`, and were garbled through Claude Code's Read path [verified]. It must
never be enabled. A test asserts a single SOS covering all components.

## Cleanup

Each image leaves three things behind [verified]:

| Where | Size | Removed by |
| --- | --- | --- |
| `<output_dir>\<session>-vN.png` (our copy) | ~2.5 MB | us, from `outputs` |
| `<CODEX_HOME>\generated_images\<threadId>\*.png` (Codex's copy) | ~2.5 MB | us (`thread/delete` leaves it [verified]) |
| `<CODEX_HOME>\sessions\…\rollout-*.jsonl` plus Codex's thread-history and name-index rows | ~7 MB (the base64 is stored twice) | `thread/delete` [verified: source] |

Nothing can shrink the rollout while the thread is alive. Only `thread/delete` removes it, and after that the
session can no longer be refined. Expiry is therefore per session [decided].

### Automatic expiry

- **When it runs.** Once per server process, at the start of the first tool call that brings up the Codex child
  (never during `initialize`). It then runs in the background, bounded to about 60 s, and never fails the call
  that triggered it.
- **What it covers.** Sessions in this project's store whose `updated` is older than `--session-ttl-days`
  (default 7; 0 disables expiry).
- **Steps per session.**
  1. Take the session lease without waiting. If another process holds it, skip the session until next time.
  2. `thread/delete {threadId}`. "no rollout found" counts as already gone. "active writer" or any other
     error skips the session until next time.
  3. Delete Codex's per-thread image folder, `<codex_home>\generated_images\<threadId>\`, with these checks:
     - `codex_home` must equal the child's `codexHome`;
     - `threadId` must be a well-formed UUID;
     - only `*.png` files and then the empty folder are removed.
  4. Delete each file in `outputs` whose exact path still exists with the recorded size. A file that was
     edited, replaced or renamed is left alone. Folders are never removed.
  5. Drop the session record. This is the last step, so a crash part-way through is retried next time.
- **Reporting.** The outcome (sessions removed, bytes freed, anything skipped and why) is logged to stderr, and
  shown in `status` under "last cleanup".

### Manual sweep

```
codex-imagegen.exe --cleanup [--older-than-days N]
```

- It runs from a terminal and needs no Claude.
- It covers every project store under the state base, not just one project. N defaults to the configured TTL;
  0 means every session that is not currently in use.
- Sessions are grouped by their `codex_home`, with one child spawned per home.
- It prints what it removed and what it skipped.
- The steps and safety checks are the same as for automatic expiry.

### Safety rules

- The server deletes only three kinds of thing:
  - the files it recorded publishing, when the path and size still match;
  - Codex's per-thread image folder for a thread it created;
  - that thread itself, through `thread/delete`.
- It never deletes by wildcard in an output directory, never deletes a folder in the project, and never
  touches a thread that is not in one of its session records.

`thread/delete` works on a thread that has turns and has already unloaded. It removes the rollout, and a
later `thread/resume` then fails with `no rollout found for thread id <id>`. Codex's
`generated_images\<threadId>` folder is left behind, which is why we remove it ourselves [verified: live, V8].

## Progress and cancellation

**Progress.**

- When the call carries `_meta.progressToken`, send `notifications/progress` about every 5 s and on each
  phase change.
- `progress` is elapsed seconds and only increases. No `total` is sent, because the percentage would be
  invented [decided].
- Messages are short phase lines, for example `generating image (23 s)`.
- Phases: starting Codex, waiting for the session to be released, resuming session, waiting for Codex,
  generating image, saving image.
- Progress is sent while holding the completion lock, so none can follow the result.

**Cancel.**

- `notifications/cancelled` is the only cancel path. It comes from Esc, from TaskStop or a kill of a
  backgrounded call, or from a client timeout. It sends `turn/interrupt {threadId, turnId}`.
- Arbitration between the cancel and the response is per request id: either the response goes out or the
  turn is interrupted, never both.
- A cancelled request gets no response.
- Any image that completed before the interrupt has already been copied and recorded, so `status` and a
  later refine see it.
- There is no cancel tool [decided]. TaskStop already covers backgrounded calls, and a foreground call blocks
  the model, which could not call a cancel tool anyway.

**What an interrupt loses.** An image call that is cut short produces no file and no `item/completed`
[assumed: V4].

**After `turn/interrupt`.**

- Wait up to about 15 s for `turn/completed`.
- If it does not arrive, return anyway, but keep the session marked busy in the registry until
  `turn/completed` or child death. No later `turn/start` can then merge into it. The lingering turn still
  counts against `--max-concurrent`, since Codex may still be generating it.
- That thread's `thread/unsubscribe` is sent when its `turn/completed` arrives, not when the call returns:
  unsubscribing earlier would also stop the notification that frees the session.
- If the turn starts after the call gave up (its `turn/start` reply came too late), interrupt that turn as soon
  as its id is known. A late reply is dropped, but the `turn/started` notification that follows it names the
  turn [verified: smoke log]. The same holds for a cancel that arrives while `turn/start` is still unanswered.
- The registry hands out each turn's interrupt once, whichever of these paths asks first.

**Timeout.** When `--timeout-seconds` runs out mid-turn, the turn is interrupted. The call returns `TIMEOUT`,
or success with a warning if an image had already completed.

## Errors

**Form.** Failures are text-only `isError:true` results, with a code, a summary, a remediation and a truncated
detail.

**Stop-and-escalate codes** carry an ACTION REQUIRED block. It says that no image was produced, that the
agent must not substitute an image made some other way (SVG, ASCII, another tool) or claim success, and that
it should relay the remediation to the user. No error is returned once an image has completed, because that
case is always a success with warnings.

| Kind | Codes |
| --- | --- |
| Stop and escalate | `CLI_NOT_FOUND`, `SPAWN_FAILED`, `APP_SERVER_FAILED`, `NOT_AUTHENTICATED`, `AUTH_EXPIRED`, `IMAGEGEN_UNAVAILABLE`, `MODEL_UNAVAILABLE`, `RATE_LIMITED`, `UPSTREAM_ERROR`, `CONTENT_REFUSED`, `IMAGE_FAILED`, `NO_IMAGE`, `TIMEOUT`, `STORE_CORRUPT`, `INTERNAL_ERROR` |
| Agent-correctable (short form) | `BAD_REQUEST`, `SESSION_EXISTS`, `SESSION_NOT_FOUND`, `SESSION_NOT_RESUMABLE`, `SESSION_BUSY`, `SESSION_OPEN_ELSEWHERE`, `TOO_MANY_RUNNING`, `CANCELLED`, `SERVER_SHUTTING_DOWN` |

**Item-level outcomes.** These apply when no image completed.

- **A `failed` item with a `failure` set** (`usageLimitExceeded`): `RATE_LIMITED`, with the `limitId` and the
  reset time, or "reset time unknown" if `resetsAt` is null.
- **A `failed` item with no `failure`:** `IMAGE_FAILED`. This covers backend refusals, 5xx errors and "no image
  data". The server cannot tell them apart, because the error text goes only to the agent model.
- **A completed turn with no image item:** `NO_IMAGE`. The likely cause is a reference or edit-target read
  failure, or the agent not calling the tool.
- **In both of those**, the agent's closing line is included in the detail, truncated and marked as untrusted
  Codex text. It is never used for classification.

**Turn-level failures** come from `TurnError.codexErrorInfo`, never from model text:

| `codexErrorInfo` | Code |
| --- | --- |
| `usageLimitExceeded`, `rateLimitExceeded`, `serverOverloaded` | `RATE_LIMITED` |
| `unauthorized` | `AUTH_EXPIRED` (and the child is recycled) |
| `cyberPolicy`, `misalignmentPolicyViolation` | `CONTENT_REFUSED` (agent-model policy, not image moderation) |
| `contextWindowExceeded`, `sessionBudgetExceeded` | `SESSION_NOT_RESUMABLE` (remediation: new session with the last image as a reference) |
| `internalServerError`, `httpConnectionFailed`, `responseStream*` | `UPSTREAM_ERROR` |
| anything else | `IMAGE_FAILED`, with detail |

## Usage display

- Every non-null window in the `codex` bucket is shown, labelled from `windowDurationMins`: 10080 is "weekly",
  300 is "5-hour", anything else is "<N>-min". A null duration gets a neutral label.
- The `codex` bucket is labelled "Codex agent usage", because it measures the agent's tokens, not images.
- An `image_gen` bucket, when one is reported (by `account/rateLimits/read` or by an update), is shown as
  "image quota".
- The cache is keyed by `limitId`. A sparse update merges only its non-null fields.

## Security posture

- **Commands.** Codex runs with a read-only sandbox, `approvalPolicy: never` and the shell tool disabled. Its
  only code execution is the code-mode `exec` isolate: V8 with no Node, no filesystem and no network, able
  only to call the nested tools that remain [verified: source]. The hardened tool list is confirmed in V1.
  Nothing asks the user for approval.
- **Reads.** Codex can read any file the user can: the image tool reads referenced paths. `reference_images`
  are checked for an image signature first.
- **Writes.** The only writes to disk are the image tool's, into `<CODEX_HOME>\generated_images`, and this
  server's new files in the output directory. The server never overwrites. It deletes only what
  [Cleanup](#cleanup) allows: the files it recorded publishing (path and size must match), Codex's image
  folder for its own threads, and those threads.
- **User configuration, in ambient mode (the default).** The user's `~/.codex/AGENTS.md` reaches the agent
  model, and nothing short of another home stops it [verified]. Everything else that could act is switched off:
  - apps and connectors
  - plugins, hooks, skills and memories
  - MCP servers, rebuilt per thread and guarded by the canary
  - `notify`
  - web search
  - the auto-review subagent
- **Dedicated home (`--codex-home <dir>`).** One-time setup: `CODEX_HOME=<dir> codex login --device-auth`.
  - It avoids everything under `~/.codex`: config, `AGENTS.md`, MCP servers, plugins, user skills.
  - Three things still load, because they live outside `CODEX_HOME` [verified]: `%USERPROFILE%\.agents\skills`,
    the machine-wide `C:\ProgramData\OpenAI\Codex` config and admin skills, and managed (MDM/enterprise)
    requirements. `status` reports a non-empty `skills/list` or a present system layer, so these are visible
    rather than assumed away.
  - Copying `auth.json` between homes is unsupported, because refresh tokens appear to be single-use.
- **Billing.** API-key variables are removed from the child, and every child's preflight requires a `chatgpt`
  account.
- **Recursion.** codex-imagegen never registers itself in Codex, and MCP servers are disabled in the child.

## MCP server details

- **Transport.** A hand-written, newline-delimited JSON-RPC 2.0 server over stdio. stdout carries protocol
  traffic only and diagnostics go to stderr. A leading BOM is stripped. Unknown methods get `-32601`.
  `resources/list`, `resources/templates/list` and `prompts/list` return empty lists.
- **Threads.** Each `tools/call` runs on its own thread, so `ping` and cancellation keep flowing.
- **Protocol versions.** Supported: `["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"]`. The server
  echoes a supported client version, and otherwise answers with the **newest**, as the MCP spec recommends.
  JSON-RPC batches, which 2025-03-26 allows, are not supported: a line holding a JSON array gets one `-32600`
  error with a null id, and none of its elements is dispatched. Only a JSON object is read as a message, on
  both the MCP side and the `app-server` side.
- **`initialize` result.** Capabilities `{tools:{}}` and short `instructions`, under 2,048 characters. Tool
  descriptions are also under 2,048 characters. No tool declares `execution.taskSupport`.
- **Shared framing.** The same framing code (send, line reader, message classification, request keys) serves
  the MCP server side and the `app-server` client side.

## Configuration

Everything is a command-line argument on the MCP entry. There is no config file of our own.

```
--codex-bin <abs path>     Codex CLI; default: PATH, then %LOCALAPPDATA%\Programs\OpenAI\Codex\bin\codex.exe
--codex-home <abs dir>     Set CODEX_HOME for the child (dedicated home; isolates ~/.codex, see Security posture)
--model <id>               Agent model, full id. Default gpt-6-astra
--effort <level>           Agent reasoning effort. Default low
--output-dir <dir>         Default output directory; relative paths resolve against CLAUDE_PROJECT_DIR
--state-dir <abs dir>      Override the per-project state directory
--timeout-seconds <n>      Whole-call limit for generate/refine. Default 300
--max-concurrent <n>       Concurrent turns across sessions. Default 4
--session-ttl-days <n>     Expire sessions idle this long (see Cleanup). Default 7; 0 disables
--doctor                   Check CLI, login, plan, capability and model from a terminal (free), then exit:
                           0 when ready, 1 otherwise
--cleanup                  Sweep expired sessions across all projects, then exit
--older-than-days <n>      With --cleanup: override the TTL (0 = every session not in use)
--help, --version
```

`CODEX_IMAGEGEN_HOME` overrides the state base. `CODEX_IMAGEGEN_BUILD`, set at compile time, stamps the commit
into `--version`.

**Registration** (user scope; works as written in PowerShell 5.1, cmd and Git Bash [verified]):

```
claude mcp add --scope user codex-imagegen -- C:\tools\codex-imagegen.exe
```

Server flags go after the exe, for example `... codex-imagegen.exe --codex-home D:\codex-home`. No Claude-side
`timeout` is needed, because calls end through `--timeout-seconds`.

## Implementation

- **Target.** Rust stable, `x86_64-pc-windows-msvc`, edition 2021. Windows-only, enforced with
  `compile_error!`. One self-contained `.exe`.
- **Release profile.** `opt-level="z"`, `lto`, `codegen-units=1`, `strip`.
- **Dependencies.** `serde` (derive), `serde_json` (`raw_value`), `png` 0.18, and `jpeg-encoder` 0.7 (default
  features, never `simd`).
  - Together these add 11 crates beyond serde, and about 200 KB of binary [verified: benchmark].
  - Base64 and the downscale are hand-written, about 110 lines, and differentially tested against the `base64`
    crate as a dev-dependency.
  - `jpeg-encoder` includes IJG-derived code, so the README carries the IJG acknowledgement.

Planned modules:

| File | Contents |
| --- | --- |
| `main.rs` | Entry point: flags, `--doctor`, the MCP loop. |
| `jsonrpc.rs` | Shared line framing, message classification, request keys, log clamping. |
| `mcp.rs` | MCP server loop, per-call threads, progress reporter, protocol negotiation. |
| `cancel.rs` | Per-request arbitration between a cancel and the response. |
| `appserver.rs` | Child supervisor: spawn in the job, handshake, pending-reply table with deadlines, routing, answers to server requests, respawn and recycle. |
| `codex.rs` | Binary resolution, preflight, per-thread config map, thread/turn parameters, developer instructions, input text. |
| `registry.rs` | Running turns per session: busy check, cap, phases, per-thread event routing, lingering-interrupt state, shutdown. |
| `session.rs` | Session store: atomic JSON, `LockFileEx` store lock and per-name leases, record type. |
| `output.rs` | Output-dir resolution and pre-check, no-replace versioned publish, automatic session names. |
| `cleanup.rs` | Session expiry and the `--cleanup` sweep, with the deletion safety rules. |
| `preview.rs` | PNG decode, area-average downscale, JPEG/PNG encode with the size steps, base64. |
| `errors.rs` | Failure contract, item-level and `codexErrorInfo` mapping. |
| `config.rs` | Flags, state directory derivation, `fnv1a64`. |
| `winjob.rs` | Job object and suspended spawn. |
| `tools.rs` | `App`, the three tools, validation and start ordering. |
| `testutil.rs` | Test temp directories. |

The estimate is about 3,500 lines without tests.

**Build and CI.**

- `build.ps1` runs fmt, `clippy -D warnings` and test. It then does a release build with remapped paths
  and a statically linked C runtime, so the exe needs no VC++ Redistributable. It stages the result to a
  gitignored `dist\` and verifies it by hash. It checks, with a self-tested check, that no user path leaked
  in and that the exe does not import `VCRUNTIME140.dll` [verified].
- CI runs on Windows. It includes a no-quota contract check: the shipped exe, given a missing `--codex-bin`,
  must return `CLI_NOT_FOUND` with an ACTION REQUIRED block.
- The tag-driven release workflow comes later.

**Unit tests (`cargo test`).** These never spawn a real Codex or call a model. The `app-server` client is
tested against a scripted fake child that replays message sequences for these cases:

- generate
- refine
- resume after restart
- "active writer" with backoff ending in `SESSION_OPEN_ELSEWHERE`
- "is closing"
- "no rollout found"
- a failed image item in a completed turn
- a completed turn with no item
- an interrupt after an image completed
- a turn failure after an image completed
- an output directory that turns unwritable after the pre-check
- a server request
- the MCP-off map, with a server added and with one removed
- an MCP canary breach
- an oversized line
- a missing `savedPath`
- a deleted edit target, where no turn is sent
- cleanup:
  - an expired session is removed completely;
  - a session whose lease is held is skipped;
  - "no rollout found" still removes the record;
  - an edited output (size mismatch) is kept;
  - a malformed `threadId` or a foreign `codex_home` deletes nothing

**`smoke.ps1`.** The only thing that spends quota: about 3 images. It is run when protocol, spawning or
session code changes, and the user is told the cost first. It drives `dist\codex-imagegen.exe` over MCP:

1. `initialize`
2. `tools/list`
3. `status`
4. `generate(session=smoke-<ts>)`, with a prompt containing quotes, a backslash, a newline and non-ASCII text
5. `refine`
6. kill the exe and start a fresh one
7. `refine` again (resume after restart)
8. `status`

It asserts:

- three files, `<session>-v1..v3.png`;
- `revisedPrompt` equals the sent text exactly on every turn;
- each result carries an `image/jpeg` preview of 512,000 bytes or less, with a long edge of 1024 px or less,
  plus the absolute path;
- `status` lists the session with three turns;
- no stray `codex` process remains.

## Verification plan

Each item must pass before the code that depends on it is considered done.

| # | Check | Cost | When |
| --- | --- | --- | --- |
| V0 | A stub MCP server returning a fixed preview from the real pipeline, plus progress: Claude Code renders the JPEG and shows the progress line. TaskStop on a backgrounded call produces `notifications/cancelled`. Moved from M1 because the preview pipeline arrives in M2; it gates M2. | Claude usage only | M2 |
| V7 | **Passed 2026-09-25.** Two app-server children on one home: B's `thread/resume` of the existing smoke thread failed with "active writer" while A held it. After A unsubscribed, `thread/closed` arrived at 5.0 s and B's resume succeeded. Both children also started threads at the same time. | free (no turn) | M1 |
| V8 | **Passed 2026-09-25.** After V7, `thread/delete` on the unloaded smoke thread (3 turns) removed the rollout; resume then failed with "no rollout found". `generated_images\<threadId>` (3 PNGs) remained. | free | M1 |
| V1 | The full spawn line in ambient mode, with `CODEX_ROLLOUT_TRACE_ROOT` set on the child. The recorded request's tools include `exec` with the nested image tool, and exclude shell, `write_stdin`, web search, browser, computer-use, multi-agent, skill and tool-suggest tools. The item is reported and `savedPath` is populated. | 1 image (part of smoke) | M2 |
| V2 | Tagged input plus developerInstructions give a verbatim `revisedPrompt` on generate and on refine with an explicit `<edit_target>`, including quotes, a backslash, a newline and non-ASCII text. | part of smoke | M2 |
| V3 | `reference_images` on generate reach `referenced_image_paths` and influence the output. | 1 image | M2 |
| V4 | `turn/interrupt` during an image call gives `turn/completed` with status `interrupted` and no file. | 1 partial image (quota effect unknown) | M4 |
| V5 | With `--codex-home` pointing at a dedicated home, images land under that home. | 1 image, plus a one-time login by the owner | M4 |
| V9 | Two codex-imagegen processes (two Claude windows) generate at the same moment. Both succeed. | 2 images | M3 |

## Milestones

| # | Scope |
| --- | --- |
| M0 | Repo scaffold: Cargo, toolchain, `.gitattributes`/`.gitignore`, `AGENTS.md` + `CLAUDE.md`, `build.ps1`, CI. |
| M1 | MCP layer, `status`, spawn, handshake and preflight (all free), the CI contract check, V7, V8. |
| M2 | `generate` end to end: pre-check, copy on item, preview, progress, errors. V0, `smoke.ps1` first version (V1, V2), V3. |
| M3 | Sessions: store, leases, `refine`, resume and unsubscribe, writer-lock handling, the full smoke, V9. Cleanup: expiry and `--cleanup`. |
| M4 | Cancel and timeout (V4), `--codex-home` (V5). |
| M5 | README: setup, a "verify it works" checklist in Claude Code, the IJG notice, the `.gitignore` tip. Release workflow when wanted. |
