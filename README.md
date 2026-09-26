# codex-imagegen

An MCP server that lets Claude Code generate images through your local Codex CLI:

1. Claude sends a prompt and gets the image back, as a preview and a file path.
2. It can continue the same Codex session to refine the image, and each refinement returns a new
   version.

Windows only. It ships as a single self-contained executable.

**Status: in development.** Nothing is usable yet. [`docs/design.md`](docs/design.md) describes what is
being built and why.

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

## Output files (planned)

Each image is saved as a full-resolution PNG, `<session>-v<N>.png`, in `generated-images\` under the
project Claude Code has open, unless a call's `output_dir` or the server's `--output-dir` names another
folder. Nothing there is ever overwritten.

That folder is scratch space, not a place to keep work: its files expire with their session after
`--session-ttl-days` idle (7 by default). Move or copy the images worth keeping into the project proper,
and keep the folder out of version control by adding this line to the project's `.gitignore`:

```
generated-images/
```

## Acknowledgements

This software is based in part on the work of the Independent JPEG Group. The JPEG encoder that builds
the previews includes code derived from the IJG's.
