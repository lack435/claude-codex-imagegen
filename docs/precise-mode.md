# Precise mode: design draft

Status: draft for the owner's review, 2026-11-23. Nothing here is built. It reopens a boundary that
`docs/design.md` and `AGENTS.md` close ("no API-key mode"), for one new, opt-in tool only. The existing tools,
`generate` and `refine`, and the Codex child are unchanged. Once approved, this folds into `docs/design.md` and
`AGENTS.md` is amended in the same change.

Claims use the tags of `docs/design.md`. Facts read in OpenAI's or Microsoft's documentation on 2026-11-23 are
**[assumed]** until a live check passes. Each such fact has an entry under [Verification](#verification).

## Why

Codex's image tool always asks for `size` and `quality` `auto`, and Codex cannot pass anything else, whether it
is signed in with ChatGPT or with an API key (`docs/design.md`, "What Codex actually provides"). The OpenAI
Images API takes those options and more. Precise mode calls that API directly from this server, billed to an
OpenAI API key, not to the ChatGPT plan.

## The boundary, stated plainly

| Today | With precise mode |
| --- | --- |
| No API-key mode. Billing is the ChatGPT plan only. | One more tool, `codex_imagegen_precise`, billed per image to an API key. It is off unless the server is registered with `--precise`. |
| API-key variables are stripped from the Codex child. | Unchanged. The precise key never enters an environment, and never reaches the Codex child or any other process. |
| The child's posture (sandbox, approvals, MCP off) | Unchanged. Precise mode does not involve Codex. |

`generate` and `refine` never use the API key, whatever flags are set [decided]. A user who never passes
`--precise` sees no difference at all: the tool is not listed, and the key is never read.

## Tool

`codex_imagegen_precise`, listed only under `--precise`. Its description starts with "Billed per image to the
user's OpenAI API key", so the agent and the user can always tell it from the plan-billed tools.

| arg | type | notes |
| --- | --- | --- |
| `prompt` | string, required | Up to 32,000 characters [assumed: P2]. Sent verbatim. |
| `session` | string, optional | A new name starts a precise session. An existing precise session edits its latest image, or `from_version`. A Codex session's name is refused: pass its image as a reference instead. |
| `from_version` | integer ≥ 1, optional | As `refine`'s. Only with an existing session. |
| `reference_images` | string[], optional | Up to 4, validated as today. Sent as edit inputs with the edit target, if any, first. |
| `size` | string, optional | `auto` (default), `1024x1024`, `1536x1024`, `1024x1536`, or a custom `WxH`: both sides multiples of 16, aspect 1:3 to 3:1, at most 3840x2160 [assumed: P2]. Validated here, before anything is spent. |
| `quality` | string, optional | `auto` (default), `low`, `medium`, `high`, plus `xhigh` and `max` where the model takes them [assumed: P2]. |
| `background` | string, optional | `auto` (default), `opaque`, `transparent`. |
| `n` | integer, optional | 1 to 4 (default 1). Each image is its own version, as when Codex makes several in one turn. |
| `moderation` | string, optional | `auto` (default) or `low`. |
| `output_dir` | string, optional | As today. |

The output is always PNG. The preview pipeline decodes PNG only (the `png` crate), and a lossy file would defeat
"precise" [decided]. Not offered in this version: `output_format`, `output_compression`, streaming and partial
images, masks and `markup`, `complexity`, GIF and SVG output.

The model is pinned by full id with `--precise-model` [decided]. The default is an open question (see
[Open questions](#open-questions)). A value outside the documented set is refused at startup, not per call.

### Generate or edit

- No session, or a new name: `POST https://api.openai.com/v1/images/generations`, JSON.
- An existing precise session, or any `reference_images`: `POST https://api.openai.com/v1/images/edits`, JSON,
  with `images: [{image_url: "data:image/png;base64,..."}]`. The edit target comes first, then the references.
  The reference page documents a JSON body with data URLs of up to 20,971,520 characters each, but its examples
  use multipart `image[]` [assumed: P3]. If JSON edits fail live, multipart (hand-written, about 100 lines) is
  the fallback.
- Responses: `data[].b64_json`, decoded, then written, recorded and previewed exactly as the Codex path
  publishes its images: versioned names, no-replace publish, fingerprint, `resolved_path`. `usage` is reported
  in the result.

## The API key

### Where it lives

In Windows Credential Manager, as a generic credential [decided]:

- target `codex-imagegen/openai-api-key`;
- `CRED_TYPE_GENERIC`, `CRED_PERSIST_LOCAL_MACHINE`, so it stays on this computer and never roams with a
  roaming profile [assumed: P1];
- the key as UTF-8 bytes in `CredentialBlob`, limited to 2,560 bytes [assumed: P1].

Protection, said plainly:

- Windows stores the credential "securely". The documentation does not name the scheme [assumed].
- It is out of other Windows users' reach, and out of the project folder, the MCP registration and every
  environment.
- It is **not** protected from other programs running as the same user. Any of them can call `CredReadW` for
  the same target, as they could read Codex's own `auth.json` today. A process already running as the user is
  out of scope, as it is for cleanup [decided].

Rejected places:

- a tool argument, which would put the key in Claude's context and transcripts;
- a server flag, which is stored in plain text in `~/.claude.json` and visible in the process list;
- `OPENAI_API_KEY`, which would collide with the rule that strips it from the Codex child, and which reaches
  every program the user starts;
- a file in the project.

### Commands

From a terminal, never through Claude:

```
codex-imagegen.exe --set-api-key      prompt for the key with echo off, check it, store it
codex-imagegen.exe --remove-api-key   delete it (CredDeleteW)
```

`--set-api-key`:

- reads from the console with `ENABLE_ECHO_INPUT` cleared, and refuses to run when stdin is not a console, so
  the key never arrives through a pipe that may have been logged;
- checks the key with a free call, `GET /v1/models` [assumed: P1], then stores it;
- prints only the last four characters.

### Use

- The key is read with `CredReadW` for each precise call and dropped when the call ends. A rotated key takes
  effect on the next call. The buffer is overwritten before `CredFree`, as best effort.
- It goes in exactly one place: the `Authorization` header of a request to `api.openai.com`.
- It never appears in stderr, `status`, a result or an error. `status` says only "API key: stored (…abcd)" or
  "not set".
- A missing key is `API_KEY_MISSING`, with the remediation "run `codex-imagegen.exe --set-api-key` in a
  terminal". No error ever asks the agent for the key, or to find one.

## Network

WinHTTP over raw FFI, like the job object: no new crate [decided].

- `WinHttpOpen` with `WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY` (Windows 8.1 and later), synchronous. Not
  `WINHTTP_FLAG_SECURE_DEFAULTS`, which forces asynchronous mode [assumed: P1].
- `WINHTTP_OPTION_SECURE_PROTOCOLS` limited to TLS 1.2 and 1.3. `WINHTTP_FLAG_SECURE` on every request.
- Certificate validation is left on: no `WINHTTP_OPTION_SECURITY_FLAGS` is ever set. A unit test greps the
  source for the `SECURITY_FLAG_IGNORE_*` names, as `build.ps1` checks the binary's imports.
- The host is the constant `api.openai.com`, port 443. There is no `--api-base` [decided], so the key cannot be
  pointed elsewhere.
- Redirects are refused (`WINHTTP_OPTION_REDIRECT_POLICY` set to never), so the `Authorization` header cannot
  follow one to another host.
- Timeouts come from `--timeout-seconds`. A cancel closes the request handle. The server may already have
  generated, and billed for, the image, and the result says so [assumed].
- Response size is capped (about 64 MiB for `n` = 4), as the Codex reader caps lines.

## Spending guardrails

- **Off by default.** The tool is listed only under `--precise`, and the key is never read without it.
- **A daily cap.** `--precise-max-images-per-day N`, counted in the state folder under the store lock, across
  processes. Each image counts when it is requested, so a failed call still counts [decided]. A call that would
  pass the cap is refused before anything is sent: `PRECISE_CAP_REACHED`.
- **On OpenAI's side**, in the README:
  - a project just for this key, with a monthly spend limit and "enforce a hard limit" turned on (the help
    article documents both; earlier forum reports call limits notification-only) [assumed: P5];
  - a restricted key, if the key dialog lets image generation be its only permission [assumed: P5].
- **Every result** says "billed to your OpenAI API key", with the `usage` tokens, and the model, size and quality
  the API reports back.

## Errors

The failure contract is unchanged. Any failure says no image was produced, and tells the agent never to
substitute one made another way. New codes:

| Code | When |
| --- | --- |
| `API_KEY_MISSING` | no stored key (stop and escalate) |
| `API_KEY_REJECTED` | 401 (stop and escalate: rerun `--set-api-key`) |
| `API_QUOTA` | 429 with `insufficient_quota`, or a spend limit hit (stop and escalate) |
| `RATE_LIMITED` | other 429s (existing code) |
| `CONTENT_REFUSED` | `moderation_blocked` (existing code; the stage, input or output, in the detail) [assumed: P4] |
| `API_ACCESS` | 403, for example organization verification required for GPT image models [assumed: P4] |
| `UPSTREAM_ERROR` | 5xx, connection or TLS failure (existing code) |
| `PRECISE_CAP_REACHED` | the daily cap (agent-correctable: tell the user) |

An `error.message` from the API is quoted, bounded and marked as untrusted. The key is never in it, and the
request is never echoed.

## Sessions and cleanup

Recommended: precise sessions live in the same store, with a new record field `engine: "api"`. A record
without the field is `"codex"`. An api record has no `thread_id` or `codex_home`.

- Cleanup deletes an api session's outputs under the existing safety rules. There is no thread to delete and no
  Codex folder.
- `refine` refuses an api session, and `codex_imagegen_precise` refuses a Codex session, each saying which tool
  owns it.
- Compatibility: an older build reading an api record ignores the new field. Its cleanup skips the record whole,
  because its empty `thread_id` is not a UUID, which is safe [verified: `docs/design.md`, Cleanup step 3]. Its
  `refine` fails at `thread/resume`, which is ugly but spends nothing.

## Tests

- **Unit tests** run against a fake transport, with no network: a trait over "send this request, return
  status, headers and body", as the Codex side tests against a fake child. They cover:
  - argument validation;
  - generate and edit request bodies, checked exactly;
  - the key placed only in `Authorization`, and absent from every log line, result and error;
  - every error code above;
  - the daily cap across two store handles;
  - `n` = 2 publishing two versions;
  - the no-redirect policy;
  - the absence of `SECURITY_FLAG_IGNORE_*`.
- **Credential Manager tests** use a throwaway target name (`codex-imagegen-test/<random>`), written, read and
  deleted within the test. Real calls, no network.
- **`smoke.ps1 -Precise`**: paid, with a stored key. One generate with size, quality and a transparent
  background, then one edit of it. It checks the published PNG's dimensions and alpha channel against what was
  asked.

## Verification

| # | Check | Cost |
| --- | --- | --- |
| P1 | Credential Manager round trip: `--set-api-key` stores, `status` shows the last 4, `--remove-api-key` deletes; the key checked with `GET /v1/models`; WinHTTP on TLS 1.2+ through the automatic proxy. | free |
| P2 | Generate with a custom size, `quality: high`, and the pinned model: the API accepts them, and the PNG has the size asked. | 1 image |
| P3 | Edit with a JSON body and a data URL. If refused, switch to multipart. | 1 image |
| P4 | Error mapping: a bad key gives 401 (free). Moderation and 403 shapes come from the documentation until one is seen. | free |
| P5 | The owner confirms in the dashboard: the restricted-key option for images, and the hard spend limit. | free |

## Open questions for the owner

1. **Default model.** Options:
   - `gpt-image-2-2026-04-21`, the dated `gpt-image-2`. Its transparency is "in preview", and it has no
     `xhigh` or `max`.
   - `gpt-image-2.5-sunburst-2026-09-08`: transparency, `xhigh` and `max`. What separates "sunburst" from
     "flare" is not stated on the pages read.
   - `gpt-image-2.6-*`, which has no dated snapshot listed, so it would be pinned to a moving alias.

   Recommendation: `gpt-image-2.5-sunburst-2026-09-08`.
2. **Same repository?** Precise mode does not use Codex. Keeping it here reuses the store, publishing, previews
   and MCP layer. A separate server would keep "codex-imagegen never holds an API key" literally true.
   Recommendation: here, behind `--precise`.
3. **Daily cap default.** Recommendation: 20 images, with 0 meaning no cap.
4. **`moderation: low`.** Expose it to the agent, or leave it at `auto` and make it a server flag?
   Recommendation: a server flag, `--precise-moderation low`, so it is the user's choice, not the agent's.
5. **Tool name.** `codex_imagegen_precise` keeps the prefix. `codex_imagegen_api` says more plainly what it costs.

## Sources

- [Create image](https://developers.openai.com/api/reference/resources/images/methods/generate/), [Create image
  edit](https://developers.openai.com/api/reference/resources/images/methods/edit/) and the [Image generation
  guide](https://developers.openai.com/api/docs/guides/image-generation), read 2026-11-23.
- [Managing projects in the API platform](https://help.openai.com/en/articles/9186755-managing-projects-in-the-api-platform).
- Microsoft Learn: [CredReadW](https://learn.microsoft.com/en-us/windows/win32/api/wincred/nf-wincred-credreadw),
  [CredWriteW](https://learn.microsoft.com/en-us/windows/win32/api/wincred/nf-wincred-credwritew),
  [CREDENTIALW](https://learn.microsoft.com/en-us/windows/win32/api/wincred/ns-wincred-credentialw),
  [WinHttpOpen](https://learn.microsoft.com/en-us/windows/win32/api/winhttp/nf-winhttp-winhttpopen),
  [WinHTTP option flags](https://learn.microsoft.com/en-us/windows/win32/winhttp/option-flags).
