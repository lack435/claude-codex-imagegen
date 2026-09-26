# codex-imagegen

An MCP server that lets Claude Code generate images through your local Codex CLI:

1. Claude sends a prompt and gets the image back, as a preview and a file path.
2. It can continue the same Codex session to refine the image, and each refinement returns a new
   version.

Windows only. It ships as a single self-contained executable.

**Status: in development.** Generating a new image works; continuing a session to refine it arrives in
the next milestone. [`docs/design.md`](docs/design.md) describes what is being built and why.

## Requirements (planned)

- The Codex CLI, installed and signed in with a ChatGPT plan that includes image generation
  (`codex login`). Image generation is billed to that plan. An OpenAI API key is not used.
- Claude Code.

## Building

```powershell
.\build.ps1
```

This runs the checks and tests, builds the release binary, and stages it at
`dist\codex-imagegen.exe`.

`.\smoke.ps1` then checks the staged binary against your real Codex CLI. On its own it runs only free
steps; `.\smoke.ps1 -SpendQuota` also generates about two images, billed to your ChatGPT plan.

## Output files

Each image is saved as a full-resolution PNG, `<session>-v<N>.png`, in `generated-images\` under the
project Claude Code has open, unless a call's `output_dir` or the server's `--output-dir` names another
folder. Nothing there is ever overwritten.

That folder is scratch space, not a place to keep work: its files expire with their session after
`--session-ttl-days` idle (7 by default; the expiry itself arrives with sessions in the next milestone).
Move or copy the images worth keeping into the project proper,
and keep the folder out of version control by adding this line to the project's `.gitignore`:

```
generated-images/
```

## Acknowledgements

This software is based in part on the work of the Independent JPEG Group. The JPEG encoder that builds
the previews includes code derived from the IJG's.
