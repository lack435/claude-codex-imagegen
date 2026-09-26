# codex-imagegen v1 design

Status: approved by the owner, 2026-09-25 (revision 3: adversarial review applied; cleanup added). M0 (repo
scaffold) is in place. M1 is implemented: the MCP layer, `status` and `--doctor`, spawn, handshake and preflight,
and the CI contract check. M2 is implemented: `generate` runs end to end (pre-check, turn, copy and preview on
each image, progress, cancellation and deadlines, errors), `status` lists running turns, and `smoke.ps1` has its
first version. M3 is implemented: the session store, its lock and the per-session leases; `generate` refuses an
existing name with `SESSION_EXISTS` and records each completed image in its session; `refine` (edit target,
`thread/resume` with its "is closing", "active writer" and "no rollout found" handling, the M2 turn engine);
cleanup (automatic expiry, once per server process, and `--cleanup`); `status` lists this project's sessions and
the last cleanup; and the full `smoke.ps1`, with V9 behind `-Concurrent`. Not built yet: recycling the shared
child after a missed per-request deadline (see Deadlines). The unit tests use a scripted fake app-server. Beyond
them, M3 has been exercised only by free runs: `--doctor`, `smoke.ps1` without `-SpendQuota` (which also runs
`--cleanup` on an empty state base), and `--cleanup` on fabricated session records whose thread ids are random
UUIDs, in the ambient home and in a scratch dedicated home. No paid run has covered refine, resume after a
restart, the cleanup of a real session, or V9 yet. The first paid `smoke.ps1` run against real Codex
(2026-09-25) passed V2 (the generate part) and V3, and its trace showed the sub-agent and `request_user_input`
tools still offered to the agent model. The spawn line now switches those off, and the second paid run passed
V1, V2 (generate) and V3 (34 of 34 automated checks). V0 passed for rendering: Claude Code received the preview as an image and described
it accurately, both through `claude -p` and in the desktop app, where it appears in the expanded tool row. The
desktop app shows no progress line; the terminal renderer draws one [verified: bundle]. The TaskStop part of V0 was
not run.

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
| `output_dir` | string, optional | Defaults to the session's recorded output directory. A folder given here applies to this call only; the record keeps the session's own [decided]. |

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
- sessions in this project, most recently updated first (up to 20): name, turns, latest output path, last
  update and its age; then the last cleanup (see [Cleanup](#cleanup)), or "not run yet". A store that cannot be
  read is reported here with the records that still parse, not failed on

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
     "the preview is a 1024px JPEG; the file is the full-resolution original\n" +
     "codex prompt: <revisedPrompt>\n" +
     "codex note: <Codex's closing line, quoted, untrusted>\n" +
     "took 38.1 s; Codex agent usage: weekly 44% (resets 2026-10-01 14:17)\n" +
     "output files are scratch and expire after 7 days idle; move keepers into the project\n" +
     "the user may not see this tool result: show them the image, by displaying or sending the file if you have a tool for that, otherwise by giving them its path"}
]
```

Formatting details [decided]:

- `codex prompt` is JSON-quoted, so a newline in it cannot read as the next line of the result. It is compared
  with the prompt sent ignoring whitespace at either end: the tags put the prompt on lines of its own, so where
  it starts and ends is the agent's reading of the layout.
- `codex note` is the last line of the agent's last message, JSON-quoted and bounded, followed by "(Codex's
  closing line, untrusted)".
- The timing line adds `image quota: …` after the agent usage when Codex has reported an `image_gen` bucket.
- If Codex makes several images in one turn, each gets its own version and preview, in order; the first line
  reads `versions: 1, 2`, each image line is labelled `image 1 of 2:`, and a warning says so.
- If the copy failed, the first line reads `version: none published (see the warnings)`.

Every success ends with a line telling the agent to show the user the image, by displaying or sending the file if
it has a tool for that, otherwise by giving its path, and the `initialize` instructions say the same [decided].
Many clients fold tool results away: in the Claude desktop app the preview appears only inside the collapsed
tool row [verified: owner, 2026-09-25], so the preview is often seen by the agent alone.

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
  2. registry try_start (busy / concurrency cap / shutdown), then the per-session cross-process lease
     (held elsewhere: SESSION_BUSY), then the store read under it (generate: an existing name is
     SESSION_EXISTS; refine: a missing one is SESSION_NOT_FOUND; a store that cannot be read is
     STORE_CORRUPT). refine: its output_dir (the argument, else the recorded one) is pre-checked as in
     step 1, and the edit target picked (see Refine)                                ── nothing spent
  3. ensure the child: spawn in a kill-on-close job → initialize → preflight (once per child). The
     first call in a server process that brings one up also starts automatic expiry (see Cleanup).
     refine: the record's codex_home must be the child's codexHome, else SESSION_NOT_RESUMABLE
  4. config/read → build the MCP-off map for this thread
  5. generate: thread/start          refine: thread/resume (with the retries under Refine)
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

**Unsubscribe.** Step 7 also follows a `thread/resume` that may have subscribed this child without a turn going
ahead: one cut short by a cancel or the end of the budget, one whose reply cannot be read, and one refused with an
error not handled under [Refine](#refine). Codex goes on with a request the client has stopped waiting for, and
runs the requests about one thread in the order they arrive, so the unsubscribe follows the late resume and the
thread unloads after its delay, releasing its writer lock [verified: source, `request_serialization.rs`,
`thread_resume_inner`]. A `thread/start` cut short the same way is not unsubscribed: the call never learns its
thread id, no session records it, and it stays loaded in the child until the child exits [decided: accepted].

**Deadlines.**

- The whole call counts against `--timeout-seconds` (default 300): spawn, preflight, resume, turn, copy and
  preview.
- Each app-server request has its own deadline: about 30 s for the handshake, preflight calls,
  `config/read`, `thread/start` and `turn/start`. `thread/resume` gets whatever remains of the call's budget.
- A missed deadline returns `APP_SERVER_FAILED`, naming the method, or `TIMEOUT` once the overall budget is
  gone.
- When the budget runs out mid-turn, the wait of up to about 15 s for the interrupted turn to complete (see
  [After `turn/interrupt`](#after-turninterrupt)) comes on top of it [decided]. That wait is what lets an image
  that finishes in those seconds still be kept, and a turn that does not confirm leave its session busy.
- The shared child is recycled only when it has no other turns running, or when a cheap liveness call
  (`config/read`) also misses a short deadline.

### Refine

1. **Pick the edit target** before Codex is started: the first of `last_saved_path` and `last_output_path` that
   exists with the recorded size (they are byte-identical copies, so `last_output_bytes` checks either). When
   the size was never learned (Codex's file could not be read as the image completed), existing is enough. If
   neither does: `SESSION_NOT_RESUMABLE`, with the remediation "start a new session with
   `generate(reference_images=[<a surviving copy>])`", naming the newest published file that still has its
   recorded size, or saying that none survives. Nothing is spent.
2. **Check the Codex home.** A record whose `codex_home` is not the child's `codexHome` gets
   `SESSION_NOT_RESUMABLE`, naming both, before any thread call. Paths compare as Windows compares them: case
   aside, a trailing separator aside, and with or without the `\\?\` prefix Codex puts on a canonicalised
   `CODEX_HOME` [verified: source, `utils/home-dir`].
3. **Always call `thread/resume`** with `excludeTurns: true` and the thread parameters, before every
   `turn/start`, each try after a fresh `config/read` and MCP-off map. Codex counts our `config` and
   `developerInstructions` as overrides it cannot apply to a loaded thread. So on a thread still loaded in this
   child with no subscriber, the normal state inside the unload delay after our unsubscribe, it shuts the idle
   thread down (waiting up to 10 s, out of the resume's budget) and resumes it cold with the fresh parameters;
   that teardown sends no `thread/closed`. Only when the thread still has a subscriber, or its shutdown fails or
   times out, does Codex rejoin the loaded thread and ignore the overrides, logging a warning [verified: source,
   `thread_processor.rs` `collect_resume_override_mismatches`, `resume_running_thread`]. The resume gets
   whatever remains of the call's budget, and every pause between tries ends at a cancellation or the end of the
   budget. A resume cut short by a cancel or the end of the budget is followed by `thread/unsubscribe` (see
   [Unsubscribe](#request-flow)). Errors, by Codex's message:
   - "is closing; retry": this child is unloading the thread. Retry every 250 ms, within the window below, then
     `APP_SERVER_FAILED`.
   - "already has an active writer": another process has the thread loaded. It may be another codex-imagegen
     still inside its unload delay. Retry with backoff (0.25 s, doubling, at most 2 s) for up to
     `thread_unload_delay_secs + 10` s (15 s), with the phase "waiting for the session to be released". Then
     return `SESSION_OPEN_ELSEWHERE`, agent-correctable: the session is open in another Claude Code window or in
     the Codex app; close it there, or use a new session.
   - "no rollout found": `SESSION_NOT_RESUMABLE`, naming the edit target as the new session's reference.
   - A config error naming `mcp_servers`: rebuild the map and retry once, as for `thread/start`.
   - Any other error: `APP_SERVER_FAILED` with the detail, after `thread/unsubscribe`, because a few of Codex's
     resume errors come after it has subscribed this child.
4. **Name the edit target explicitly** in the input. Codex never picks it from memory [decided].
5. **Run the turn** exactly as generate's: the canary check before `turn/start`, each image published as
   `<session>-v<next_version>.png` (bumped past a taken name) and recorded at once, the unsubscribe, cancel,
   deadlines and lingering. `revisedPrompt` is compared with the feedback, and a difference is a warning. A turn
   that fails with `contextWindowExceeded` or `sessionBudgetExceeded` is `SESSION_NOT_RESUMABLE`, naming the
   edit target as the new session's reference.

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
  --disable multi_agent_v2 --disable goals --disable shell_tool --disable tool_suggest
  --disable skill_search --disable browser_use --disable computer_use --disable in_app_browser
  -c notify=[] -c skills.bundled.enabled=false -c skills.include_instructions=false
  -c agents.enabled=false -c tools.experimental_request_user_input.enabled=false
  -c web_search="disabled" -c approvals_reviewer="user" -c windows.sandbox="unelevated"
  -c thread_unload_delay_secs=5
```

**What has been checked.**

- Every switch was accepted, and `config/read` shows it in effect [verified]. One is shown differently:
  `config/read`'s `config` carries only `web_search` under `tools` (app-server-protocol `ToolsV2`), so
  `tools.experimental_request_user_input.enabled` appears only in the reply's `origins`, as taken from the
  session-flags layer, which holds our `-c` [verified: `config/read` on 0.156.0; source].
- For apps, plugins, bundled skills, notify and MCP servers, the effect also shows in `skills/list`,
  `plugin/list` and `mcpServerStatus/list` [verified].
- The first paid smoke run (2026-09-25, before the three switches below were added) recorded the tools the
  agent model was offered [verified: rollout trace]:
  - `exec`, with `apply_patch`, `view_image`, `clock__curr_time` and `image_gen__imagegen` nested in it;
  - `wait` (code mode's wait on a running `exec` cell), `request_user_input` and `request_user_input_async`;
  - `sleep`, in a `clock` namespace;
  - six sub-agent tools in a `collaboration` namespace: `spawn_agent`, `send_message`, `followup_task`,
    `wait_agent`, `list_agents` and `interrupt_agent`;
  - no shell, stdin, web search, browser, computer-use, skill, tool-suggest or MCP tool.
- **Sub-agent tools.** `--disable multi_agent` does not remove them. The model catalogue gives `gpt-6-astra`
  MultiAgentV2, which applies unless `agents.enabled = false`, and an enabled `multi_agent_v2` feature
  outranks even that [verified: source, `core/src/config/mod.rs` `multi_agent_version_override`]. So both are
  switched off: under `agents.enabled=false` alone, a user's `[features.multi_agent_v2] enabled = true` stayed
  in effect, and `--disable multi_agent_v2` turned it off [verified: `config/read` on a test home].
- **`request_user_input`** is offered unless `tools.experimental_request_user_input.enabled = false`
  [verified: source, `core/src/config/mod.rs`, `core/src/tools/spec_plan.rs`].
- **`request_user_input_async` stays.** It is offered because the catalogue lists `send_user_message_async` for
  `gpt-6-astra`, and no config switch removes it. It only records an agent message holding the questions and
  returns at once; it never waits for an answer and sends no request to this server [verified: source,
  `core/src/tools/handlers/request_user_input_async.rs`] [decided: accepted].
- **`apply_patch` stays.** It is nested in `exec` for every model while the thread has an environment, and the
  environment cannot go without breaking `referenced_image_paths`. Under the read-only sandbox with
  `approvalPolicy: never`, Codex rejects every patch [verified: source, `core/src/safety.rs`] [decided: accepted].
  `view_image` and the clock tools are harmless and allowed.
- That the sub-agent and `request_user_input` tools are gone from the offered list under the new switches is
  known from source and `config/read` only [assumed: V1].
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
   in effect: `features.image_generation` true, each switched-off feature false (a feature that also takes
   settings may read as a table, which counts when its `enabled` is false [verified: `config/read`]),
   `web_search`, `notify`, `skills.*`, `agents.enabled`, `approvals_reviewer` and `windows.sandbox` as set.
   `tools.experimental_request_user_input.enabled`, which `config` leaves out, must have the session-flags
   layer (our `-c`) as its entry in the reply's `origins`. A legacy alias of a switched-off feature
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

**When a turn fails with `AUTH_EXPIRED`.** The child is marked for recycling: the next call starts a fresh one,
and this one is closed once no call is using it. Each call running a turn holds the child, so it is closed at
once when nothing else holds it, and otherwise when the last such call finishes, which drops it and with it the
job [decided]. A lingering turn on it cannot complete once it is closed, so its session is freed. Until it is
closed, the server keeps a weak reference to it, so shutdown still reaches it and its turns (see
[Lifecycle](#lifecycle)). The same holds for a child replaced because `account/updated` reported another login.

**`account/updated`.** An `authMode` other than `chatgpt` invalidates the cached preflight.

### Thread and turn parameters

- **Before every `thread/start` and `thread/resume`**, call `config/read` (local and free) and pass
  `config:{mcp_servers:{<each name present>:{enabled:false}}}`. The map is rebuilt from that read every time,
  so a server the user adds mid-session never loads. A server that was removed never leaves a stale entry
  behind, which would otherwise fail every later config build [verified: source].
- **The same read re-checks the spawn switches** (preflight step 4) and fails with `IMAGEGEN_UNAVAILABLE`
  naming the setting [decided]: the configuration can change under a running child, and a thread started now
  reads it now.
- **If `thread/start` fails** with a config error mentioning `mcp_servers`, re-read the config, rebuild the map
  and retry once.
- **`thread/start`:**
  - `model`: `gpt-6-astra`, full id [decided]
  - `cwd`: the work directory
  - `sandbox`: `"read-only"`
  - `approvalPolicy`: `"never"`
  - `approvalsReviewer`: `"user"`
  - `developerInstructions`
  - `config`: the MCP-off map
  - `ephemeral`: false
- **`thread/resume`:** the same, less `ephemeral` (`ThreadResumeParams` has no such field, and the thread was
  created persistent), plus `threadId` and `excludeTurns: true`.
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
- Codex reports MCP server startups right behind the `thread/start` reply [verified: smoke log], which can be
  before the call has had that reply and registered its thread. Those notifications (the canary's) are held
  briefly, a bounded few, and handed to the call when it registers the thread, so none is lost in the gap.
- The reader thread only routes. Copies and previews run on the call's own thread, so one session's 3 MB copy
  never stalls another session's messages.
- `turn/interrupt` and `thread/unsubscribe` are sent without waiting for their replies: the interrupt from a
  cancel hook on the MCP reader thread, or from the notification handler on the app-server reader thread, neither
  of which may wait. A reply that reports an error is logged.

**Image items.**

- Check `status` before reading anything else.
- Only a `completed` item with a `savedPath` is copied.
- A `completed` item with no `savedPath` is re-parsed with `result: Cow<str>`, and the base64 is decoded
  instead. The result reads the same either way, so each completed item logs one stderr line naming its
  `savedPath`, or saying it had none; `smoke.ps1` reads it for V1.
- A `completed` item with neither a `savedPath` nor decodable image data leaves no image anywhere, so it counts
  as a failed item [decided].
- A `failed` item is handled under [Errors](#errors).
- A notification about one of our threads that cannot be parsed is a protocol anomaly. If no image completed and
  nothing else explains it, the call fails with `APP_SERVER_FAILED`, naming the Codex version.

**Server-to-client requests.** These include approvals, `requestUserInput`, elicitation and
`chatgptAuthTokens/refresh`. Each is answered at once with JSON-RPC error `-32601` and logged, so a turn can
never hang [decided].

**Canary.** `mcpServer/startupStatus/updated` is deliberately *not* opted out. A disabled server reports no
startup status at all [verified: source, `codex-mcp` `connection_manager.rs` starts only enabled servers], so any
report on one of our threads means a server is starting there, whether or not it is in the disabled map (a
`disabled` status is let through, in case a later Codex reports one). The call then fails with
`APP_SERVER_FAILED` ("isolation breach: MCP server <name> started"). The report usually arrives before the turn
starts; the call checks for it just before `turn/start`, and then starts no turn at all, so the agent model never
sees the server's tools. A report that comes later interrupts the turn.

**Usage updates.** `account/rateLimits/updated` is merged into the usage cache under its `limitId` (see
[Usage display](#usage-display)).

### Lifecycle

- **Child death.** In-flight turns fail with `APP_SERVER_FAILED`, unless an image already completed (see the
  success rule). The next call respawns the child and re-runs preflight. The exit can be seen before the
  reader has read the child's last lines, an image's `item/completed` among them, so a call that finds the
  child dead first waits, bounded to about 2.5 s, for the reader to reach the end of the output (it marks
  that only after routing every line), then handles every event already routed to it [decided].
- **Our stdin closing.** When our stdin closes and we are given time to exit:
  1. interrupt running turns, each through the child running it. Besides the current child, that can be a
     retired one (see [When a turn fails with `AUTH_EXPIRED`](#handshake-and-preflight)) that a call still
     holds; each child carries an id, and the registry records which child each turn runs on;
  2. let copies already in progress finish;
  3. end the input of every live child, the current one and each retired one: the writer thread writes what is
     still queued, then closes the pipe (an idle child exits in 0.05–0.07 s [verified]);
  4. wait up to 5 s in total, one grace shared by all of them;
  5. drop the jobs. This step never waits on the pipe, so a child that stopped reading is killed on time. A call
     whose child is gone returns soon after, bounded by the ~2.5 s exit settle (see Child death), so no call
     outlives the grace by more than that.

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
- **The file.** `{version: 1, sessions: {<lowercased name>: <record>}, last_cleanup?}`. Records are keyed by the
  lowercased name, because names compare case-insensitively. `last_cleanup` is the outcome of the last cleanup
  that covered this store, `{at, removed, freed_bytes, skipped:[{name, why}]}`, shown by `status`. A file with
  another `version` is refused like one that cannot be parsed, since rewriting it would drop what a newer build
  keeps there [decided].
- **Record fields.** `{name, thread_id, codex_home, model, created, updated, turns, last_saved_path,
  last_output_path, last_output_bytes, output_dir, next_version, outputs:[{version, path, bytes}]}`.
  - `name` is the name as the session was created; file names keep that spelling.
  - `codex_home` is the child's `codexHome` from its handshake. `created` and `updated` are Unix seconds.
  - `turns` counts turns that completed at least one image.
  - `last_saved_path` and `last_output_path` are Codex's copy and ours of the latest image; either is null when
    that copy does not exist (no `savedPath`, or a failed copy), never an older version's path.
    `last_output_bytes` is that image's size, which checks either copy.
  - `outputs` lists every file this server published for the session. [Cleanup](#cleanup) uses it.
- **When the record is written.** When the session's first image completes, not at turn end. It is updated on
  every later completed image, in stream order. A turn that produces no completed image leaves a new name free,
  and leaves an existing record unchanged. A record is never taken over by a call on another thread. A write
  that fails after the image was delivered is a `warning:` on the success, saying refine may not find the
  session.
- **How it is written.** Atomically: a temp file in the same folder (`.sessions.<pid>-<seq>.tmp`), flushed, then
  renamed over the live file with retry; the live file is never deleted first. A `LockFileEx` byte-range lock on
  the sibling `sessions.lock` serialises every read-modify-write, and the OS releases it if the process dies. A
  writer waits up to 10 s for it; one held longer belongs to a stuck process, and the write fails. Plain reads
  take no lock, because the rename already makes them safe.
- **How it is read.** `generate` and `refine` fail closed: a store that exists but cannot be parsed (or read) is
  `STORE_CORRUPT`, and nothing is spent, because the record written later would replace sessions the call cannot
  see. It is never overwritten or moved aside automatically. `status` reads tolerantly: it lists the records
  that still parse and says what is wrong. A missing store is an empty one.
- **Two kinds of exclusion.**
  - The per-session **lease** (a `LockFileEx` lock on `<state dir>\locks\<name>-<fnv64(lowercased name)>.lock`,
    held for the whole call) stops a same-name generate from spending quota twice across processes: it is taken
    before the record is read. It also keeps the record's read, turn and write in order. A lease held by
    another process is not waited for: the call gets `SESSION_BUSY` at once, as a busy session in this process
    does [decided]. The readable part of the lease file name keeps only letters, digits, `-` and `_`.
  - Lock files are never deleted. They are opened without delete sharing, so a held one cannot be deleted and
    replaced by a fresh file that a second process would then lock. So `locks\` keeps one empty file per session
    name ever used [decided: accepted].
  - **Cross-process exclusion on the thread itself** comes from Codex's writer lock. We release it by
    unsubscribing after every turn, so a session created in one Claude Code window can be refined from another
    about 5 s later.
  - Within one process, the registry refuses a second call on a busy session with `SESSION_BUSY`, so a
    `turn/start` never merges into a running turn.
- **Concurrency.** Different sessions run concurrently on the same child. The global cap is `--max-concurrent`
  (default 4), beyond which calls get `TOO_MANY_RUNNING` [decided].
- **A different Codex home.** A record whose `codex_home` differs from the child's `codexHome` gets
  `SESSION_NOT_RESUMABLE`.
- **The session's folder.** `output_dir` is the folder the session was created with. A refine that names
  another folder publishes there, and the record keeps its own [decided]; cleanup finds every file through
  `outputs` either way.
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

- **When it runs.** Once per server process, when the first tool call (any of the three) brings up the Codex
  child (never during `initialize`, and never for `--doctor`). It runs on a thread of its own, through that
  child, bounded to about 60 s (each `thread/delete` to 10 s), and never delays or fails the call that
  triggered it. It holds the child only weakly, so a child retired meanwhile is not kept alive by it, and it
  stops at shutdown.
- **What it covers.** Sessions in this project's store whose `updated` is older than `--session-ttl-days`
  (default 7; 0 disables expiry). A store that cannot be read is left alone and the reason logged.
- **Steps per session.**
  1. Skip a session busy in this process (a returned call's interrupted turn can still hold it in the
     registry after its lease is released), then take the session lease without waiting. If another process
     holds it, skip the session until next time.
  2. Read the record again under the lease. One that a call refreshed since the listing, or that is gone, is
     left alone.
  3. A record whose `codex_home` is not the child's `codexHome`, or whose `threadId` is not a well-formed UUID,
     is skipped whole: nothing of it is deleted, not even its thread (`--cleanup` covers other homes).
  4. `thread/delete {threadId}`. "no rollout found" (and "thread not found") counts as already gone: for a
     thread id Codex does not know, `thread/delete` answers `no rollout found for thread id <id>`, in the ambient
     home and in a fresh dedicated one [verified: live, `--cleanup` on fabricated records, 2026-09-25]. "active
     writer" or any other error skips the session until next time.
  5. Delete Codex's per-thread image folder, `<codex_home>\generated_images\<threadId>\`: only `*.png` entries
     that are plain files, then the folder once it is empty. A folder that is a link (a reparse point) is left
     alone. Anything else in it keeps the folder, not the session.
  6. Delete each file in `outputs` whose exact path is still a plain file with the recorded size, under the name
     it was published with (`<name>-v<version>.png`). A file that was edited, replaced or renamed is left alone.
     Folders are never removed.
  7. Drop the session record, last, so a cleanup cut short is retried next time. A deletion in steps 5 or 6 that
     fails (as opposed to a file left alone on purpose) keeps the record for the same reason; the thread is
     already gone then, so the next run's `thread/delete` answers "no rollout found" and the rest follows.
- **Reporting.** The outcome (sessions removed, bytes freed, anything skipped and why) is logged to stderr, and
  kept as the store's `last_cleanup` for `status` under "last cleanup", also when nothing was due. "Freed"
  counts only the files codex-imagegen deleted itself, not the rollout `thread/delete` removed.

### Manual sweep

```
codex-imagegen.exe --cleanup [--older-than-days N]
```

- It runs from a terminal and needs no Claude.
- It covers every project store under the state base (each folder directly under it that holds a
  `sessions.json`), plus `--state-dir`'s when that lies elsewhere. N defaults to the configured TTL; 0 means
  every session that is not currently in use. With `--session-ttl-days 0` and no `--older-than-days`, nothing
  is removed.
- Sessions are grouped by their `codex_home`, with one child spawned per home: the default child, started as a
  server here would start it (ambient, or `--codex-home`), for its own home, and one with `CODEX_HOME` set to
  each other home. A child whose handshake reports another home than the one asked for deletes nothing: its
  home's sessions are skipped.
- These children use the server's spawn line and are initialized, but not preflighted: deleting a thread is
  local to the home, needs no sign-in or model and runs no turn, so a signed-out home can still be cleaned
  [decided].
- It prints what it removed and what it skipped, and records each store's outcome as its `last_cleanup`. It
  exits 0, or 1 when a store could not be read or a home could not be reached. A session skipped because it is
  in use is normal.
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
- The cancel hook sends the interrupt itself, so Codex can confirm it before the call's own thread has noticed
  the cancellation. A cancellation therefore counts however the turn ended.
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
- `turn/completed` can arrive just as the call gives up, after its last look at its events but before it leaves
  the session lingering. No second one would come, so the registry records a `turn/completed` whenever it routes
  one, and a call giving up on a turn that has already completed frees the session at once and sends the
  unsubscribe itself.
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

`STORE_CORRUPT` covers a session store that exists but cannot be parsed (the remediation: move it aside, and a
fresh store starts), and one that cannot be used at all: its folder, its lock or a lease failed with an I/O error,
or another process held the store lock past its wait. `SESSION_BUSY` covers a session busy in this process and
one whose lease another process holds.

**Which failure is reported** when no image completed, first match wins [decided]: an isolation breach; a
cancellation; the child dying; a failed turn (by its `codexErrorInfo`); an exhausted image quota; another failed
image item; the budget running out; a protocol anomaly; then the turn's end state (`NO_IMAGE` for a completed turn
that never started an image call, `IMAGE_FAILED` for one that started it but never completed it).

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
  only to call the nested tools that remain [verified: source]. `apply_patch` is among them, offered to every
  model, but the read-only sandbox with `approvalPolicy: never` refuses every write it attempts [verified:
  trace + source]. The sub-agent and `request_user_input` tools are switched off (see [Spawn](#spawn)); that
  they are gone from the offered list is re-checked in V1. `request_user_input_async` remains and cannot wait
  for an answer. Nothing asks the user for approval.
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
  - sub-agents and `request_user_input`
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
| `appserver.rs` | Child supervisor: spawn in the job, handshake, pending-reply table with deadlines, detached requests, routing, answers to server requests, shutdown. |
| `codex.rs` | Binary resolution, preflight, per-thread config map, thread/turn parameters, developer instructions, input text. |
| `registry.rs` | Running turns per session: busy check, cap, phases, per-thread event routing (holding a canary that beats the thread's registration), lingering-interrupt state, shutdown. |
| `turn.rs` | One image turn: notification parsing and routing on the reader thread; on the call's thread `thread/start` or `thread/resume` (with its retries), `turn/start`, copy and preview per image, cancel hook, deadlines, canary, unsubscribe; result building and the no-image failure choice. |
| `session.rs` | Session store: atomic JSON, strict and tolerant reads, `LockFileEx` store lock and per-name leases, record type, and the per-call writer that records each completed image. |
| `output.rs` | Output-dir resolution and pre-check, no-replace versioned publish, automatic session names. |
| `cleanup.rs` | Session expiry and the `--cleanup` sweep, with the deletion safety rules. |
| `preview.rs` | PNG decode, area-average downscale, JPEG/PNG encode with the size steps, base64. |
| `errors.rs` | Failure contract, item-level and `codexErrorInfo` mapping. |
| `config.rs` | Flags, state directory derivation, `fnv1a64`. |
| `winjob.rs` | Job object and suspended spawn. |
| `tools.rs` | `App`, the three tools, validation and start ordering, refine's edit target and home check, the child's notification handler, child retirement, the start of automatic expiry. |
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
- a child seen to exit before its last image line is read, and one that exits while the call is busy with an
  earlier image
- a deleted edit target, where no turn is sent
- sessions, through generate: the record written as the first image completes (not at turn end) and not when no
  image completed; `next_version` after a taken file name; several images in one turn; a failed copy; an existing
  name, whatever its case, refused with `SESSION_EXISTS` before Codex starts; a lease held by another handle
  refused with `SESSION_BUSY` before Codex starts; an unreadable store refused with `STORE_CORRUPT` while `status`
  still reports; a record write that fails as a warning on the success; automatic names drawn again while taken
- the store itself: concurrent writers on separate handles losing no update while readers never see a partial
  file; a store that cannot be parsed never overwritten; the tolerant read keeping the records that parse; another
  store version refused; leases excluding each other across handles whatever the case; the store lock's wait
- refine: the exact `thread/resume` parameters and the tagged edit input; the recorded output folder, and an
  `output_dir` argument for one call; the edit target falling back to the published copy, versions carrying on
  past a taken name; no copy left (nothing sent, a surviving older version named); a foreign `codex_home`;
  "is closing" retried; "active writer" waited out, and ending in `SESSION_OPEN_ELSEWHERE`; "no rollout found"
  and another refusal (unsubscribed); a resume abandoned on a cancel or a timeout, then answered late, followed by
  `thread/unsubscribe`; `contextWindowExceeded`; a revised prompt that is not the feedback; a turn with no image;
  the canary after a resume; a held lease and an unreadable store before Codex starts; a generated session
  refined after Codex restarts
- cleanup:
  - an expired session is removed completely;
  - the record is dropped last, so a deletion that fails is retried;
  - a session whose lease is held, or that is busy in this process, is skipped;
  - "no rollout found" still removes the record, and "active writer" skips it;
  - an edited, replaced or renamed output is kept;
  - a malformed `threadId` or a foreign `codex_home` deletes nothing;
  - a session refreshed before its lease was taken is left alone;
  - automatic expiry runs once per process, and `--session-ttl-days 0` turns it off;
  - the sweep covers every project store with one child per Codex home, reports an unreadable store and a home
    that answers for another path, and does nothing when expiry is off and no age is given

**`smoke.ps1`.** The only thing that spends quota, and only with its `-SpendQuota` switch. It drives
`dist\codex-imagegen.exe` over MCP, as Claude Code does, and every server it starts runs with
`CODEX_IMAGEGEN_HOME` pointing at a fresh state folder in the run's working folder, with its own working
directory and no `CLAUDE_PROJECT_DIR`, so the sessions it makes and removes are its own, never the user's.

Without `-SpendQuota` it runs the free steps: `initialize`, `tools/list`, `status` (which must show no sessions
under that state folder), the no-stray-process check after closing the server's stdin, then `--cleanup
--older-than-days 0` on the state folder, which must exit 0 with nothing to remove.

The M3 version spends about 3 images, and is run when protocol, spawning or session code changes, after the
user is told the cost:

1. `initialize`, `tools/list`, `status`
2. (a) `generate(session=smoke-<ts>)`, with a prompt holding quotes, a backslash, a newline and non-ASCII text
3. (b) `refine`, with feedback holding the same kinds of character
4. (c) kill the exe (the job object must take the Codex tree with it), start a fresh one on the same state
   folder, and `refine` again: resume after a restart
5. (d) `status`, which must list the session with three turns and its latest file; then close the server
6. (e) free, and run whatever the steps before got to: `--cleanup --older-than-days 0` on the state folder

Each result must carry an `image/jpeg` preview of 512,000 bytes or less with a long edge of 1024 px or less, be
the next version (`session: <name>   version: N`), name `<session>-vN.png` in the output folder, and that file
must exist. V2 is compared by the script itself on every turn: the `codex prompt` line, JSON-decoded, must equal
the prompt or feedback sent (whitespace at either end aside), and the server must raise no prompt warning.

Step (e) covers every session the isolated store records, not only those the run kept track of: a result lost
after its image completed still left a record, and a thread in the real Codex home that nothing else would
remove. Before it, each session's record must list exactly the files the run published, named independently of
the record (`<session>-v1..vN`: the output folder is fresh), and those files must exist; a recorded session the
run lost track of fails the run. After it, each session's record, those files, any other file its record listed
or of its name in the output folder, Codex's image folder for its thread and the thread's rollout (found by its
id under `<codex_home>\sessions`, which must exist before) must all be gone, and `--cleanup` must exit 0 and
report the session removed.

With `-Concurrent` as well (V9, about 2 more images), two server processes on the same state folder, each with
its own stdin, get their `generate` requests before either is answered; both must succeed, and both sessions
must be recorded in the shared store. Step (e) then removes them too.

The servers run with `CODEX_ROLLOUT_TRACE_ROOT` set, which the child inherits, and the script reads the trace.
Each thread writes a `trace-*` folder holding `trace.jsonl` and the payload files its events name [verified:
real trace, 0.156.0]:

- V1: the offered tools, from every model request in every trace folder. A full request carries them in its
  first input item (`additional_tools`), grouped in namespaces; a follow-up request in the same turn carries only
  the new input, the image's multi-MB result among it, and no tools, so it is not parsed. The tools nested in
  `exec` are the "### `name`" headings in its description, not the calls it shows as examples. V1 passes when
  `exec` is offered with `image_gen__imagegen` nested in it, and no shell, stdin, web-search, browser,
  computer-use, sub-agent (the `collaboration` namespace or its tool names), `request_user_input`, skill,
  tool-suggest or MCP tool is offered. `apply_patch`, `view_image`, `wait`, the clock tools and
  `request_user_input_async` are listed as allowed (documented; see [Spawn](#spawn)), and anything else fails
  V1 until it is classified. A model request whose payload is missing or cannot be parsed also fails V1, and the
  tool-surface checks are then not run: the tools that request carried were never checked. V3 is still run.
- V3: an image call is a `tool_call_started` event of kind `image_generation`. Its `referenced_image_paths` come
  from its invocation payload, or, if the trace has none, from the JavaScript of the `exec` cell that made it
  (the matching `code_cell_started` event's `source_js`, JavaScript string escapes decoded). Some call must list
  the first image (full path, compared case-insensitively): for the refine in (b), its edit target, which is
  Codex's copy of the first image (its `savedPath`, from the server's stderr) or, failing that, ours; either
  counts. The event's `input_preview` is truncated and never
  used. Text Codex sent to the model never counts: the developer instructions, the tool's declaration and the
  tagged input name both the parameter and the path whatever the agent does.
- The tool results, which carry each image's base64, are never read.

`smoke.ps1 -CheckTrace <trace folder> [-ReferencePath <png>[,<png>...]]` runs only these two checks against the
trace of an earlier run; any of the paths given counts for V3. It starts nothing and spends nothing.

V1's `savedPath` comes from the servers' stderr: each image must have logged its `savedPath`, under
`<codex_home>\generated_images`, byte-identical to the published copy (a session's k-th image is its version k),
and none may have fallen back to the base64. These checks run before (e), which deletes what they compare. The
no-stray-process check covers every process of each server alive just before it stops, whether it was closed
or killed.

## Verification plan

Each item must pass before the code that depends on it is considered done.

| # | Check | Cost | When |
| --- | --- | --- | --- |
| V0 | **Passed 2026-09-25 for rendering.** Claude Code renders the JPEG preview and shows the progress line; TaskStop on a backgrounded call produces `notifications/cancelled`. Run against the real server rather than a stub. Through `claude -p` (1 image), Claude described the preview accurately and reported it intact, matching the saved file. In the desktop app's Code tab the image appears in the expanded tool row, the text block is shown above it (so the preview note is worded order-neutrally), and no progress line is drawn; the terminal renderer draws progress [verified: bundle]. The TaskStop part was not run. | 1 image + Claude usage | M2 |
| V7 | **Passed 2026-09-25.** Two app-server children on one home: B's `thread/resume` of the existing smoke thread failed with "active writer" while A held it. After A unsubscribed, `thread/closed` arrived at 5.0 s and B's resume succeeded. Both children also started threads at the same time. | free (no turn) | M1 |
| V8 | **Passed 2026-09-25.** After V7, `thread/delete` on the unloaded smoke thread (3 turns) removed the rollout; resume then failed with "no rollout found". `generated_images\<threadId>` (3 PNGs) remained. | free | M1 |
| V1 | **Passed 2026-09-25** (second paid run, after the sub-agent and user-input switches). The full spawn line in ambient mode, with `CODEX_ROLLOUT_TRACE_ROOT` set on the child. The recorded requests offered only `functions.exec` (nested: `image_gen__imagegen`, plus the documented `apply_patch`, `view_image`, `clock__curr_time`), `functions.wait`, `functions.request_user_input_async` and `clock.sleep`: no shell, `write_stdin`, web search, browser, computer-use, sub-agent, `request_user_input`, skill, tool-suggest or MCP tool, and nothing unclassified. The item was reported with `savedPath` populated, and each published file is a byte copy of it. The first paid run had shown the sub-agent (`collaboration`) and `request_user_input` tools still offered, which the added switches removed. | 1 image (part of smoke) | M2 |
| V2 | **Passed 2026-09-25 for generate.** Tagged input plus developerInstructions give a verbatim `revisedPrompt` on generate and on refine with an explicit `<edit_target>`, including quotes, a backslash, a newline and non-ASCII text. The generate prompt with quotes, a backslash, a newline and non-ASCII text came back verbatim. The refine part is built into the M3 `smoke.ps1` and has not been run yet. | part of smoke | M2 |
| V3 | **Passed 2026-09-25.** `reference_images` on generate reach `referenced_image_paths` and influence the output. The reference reached `referenced_image_paths` (trace), and the output followed it. | 1 image | M2 |
| V4 | `turn/interrupt` during an image call gives `turn/completed` with status `interrupted` and no file. | 1 partial image (quota effect unknown) | M4 |
| V5 | With `--codex-home` pointing at a dedicated home, images land under that home. | 1 image, plus a one-time login by the owner | M4 |
| V9 | Two codex-imagegen processes (two Claude windows) generate at the same moment. Both succeed. `smoke.ps1 -SpendQuota -Concurrent` runs it; not run yet. | 2 images | M3 |

## Milestones

| # | Scope |
| --- | --- |
| M0 | Repo scaffold: Cargo, toolchain, `.gitattributes`/`.gitignore`, `AGENTS.md` + `CLAUDE.md`, `build.ps1`, CI. |
| M1 | MCP layer, `status`, spawn, handshake and preflight (all free), the CI contract check, V7, V8. |
| M2 | `generate` end to end: pre-check, copy on item, preview, progress, errors. V0, `smoke.ps1` first version (V1, V2), V3. |
| M3 | Sessions: store, leases, `refine`, resume and unsubscribe, writer-lock handling, the full smoke, V9. Cleanup: expiry and `--cleanup`. |
| M4 | Cancel and timeout (V4), `--codex-home` (V5). |
| M5 | README: setup, a "verify it works" checklist in Claude Code, the IJG notice, the `.gitignore` tip. Release workflow when wanted. |
