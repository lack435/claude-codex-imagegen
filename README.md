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
