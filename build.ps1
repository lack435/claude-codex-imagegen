<#
.SYNOPSIS
  Build codex-imagegen.exe and stage it in dist/ for local use.

.DESCRIPTION
  Runs fmt, clippy and the tests, then builds the release binary and copies it to
  dist/codex-imagegen.exe. That copy is gitignored: releases are published by CI, and this
  staging path exists so a local build has one stable place to come from.
#>
[CmdletBinding()]
param(
    # Skip fmt/clippy/tests and just build.
    [switch]$SkipChecks
)

# Deliberately not 'Stop': cargo writes ordinary progress to stderr, and under
# Windows PowerShell 5.1 that would be promoted to a terminating error. Each step is
# gated on $LASTEXITCODE instead, which is the actual signal.
$ErrorActionPreference = 'Continue'
Set-Location $PSScriptRoot

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    $cargoHome = Join-Path $env:USERPROFILE '.cargo\bin'
    if (Test-Path (Join-Path $cargoHome 'cargo.exe')) {
        $env:PATH = "$cargoHome;$env:PATH"
    }
    else {
        throw "cargo was not found. Install Rust from https://rustup.rs and retry."
    }
}

if (-not $SkipChecks) {
    Write-Host '==> cargo fmt --check' -ForegroundColor Cyan
    cargo fmt --check
    if ($LASTEXITCODE -ne 0) { throw "formatting check failed; run 'cargo fmt'" }

    Write-Host '==> cargo clippy' -ForegroundColor Cyan
    cargo clippy --all-targets -- -D warnings
    if ($LASTEXITCODE -ne 0) { throw 'clippy reported problems' }

    Write-Host '==> cargo test' -ForegroundColor Cyan
    cargo test
    if ($LASTEXITCODE -ne 0) { throw 'tests failed' }
}

# Strip local absolute paths out of the binary. rustc embeds the source path of every
# crate for panic locations, so an unremapped build ships the building user's home
# directory -- e.g. C:\Users\<you>\.cargo\registry\... -- inside a binary that gets
# published. This runs in CI too, so a release carries the runner's paths no more than a
# local build carries yours.
#
# CARGO_ENCODED_RUSTFLAGS rather than RUSTFLAGS because the latter is space-separated,
# which would split a home directory containing a space. The separator is a literal 0x1f.
$cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE '.cargo' }
$rustupHome = if ($env:RUSTUP_HOME) { $env:RUSTUP_HOME } else { Join-Path $env:USERPROFILE '.rustup' }
$unit = [char]0x1f
$env:CARGO_ENCODED_RUSTFLAGS = @(
    "--remap-path-prefix=$cargoHome=/cargo"
    "--remap-path-prefix=$rustupHome=/rustup"
) -join $unit

Write-Host '==> cargo build --release (paths remapped)' -ForegroundColor Cyan
cargo build --release
if ($LASTEXITCODE -ne 0) { throw 'release build failed' }

$built = Join-Path $PSScriptRoot 'target\release\codex-imagegen.exe'
if (-not (Test-Path $built)) { throw "expected binary not found at $built" }

$distDir = Join-Path $PSScriptRoot 'dist'
if (-not (Test-Path $distDir)) { New-Item -ItemType Directory -Path $distDir | Out-Null }
$dist = Join-Path $distDir 'codex-imagegen.exe'
Copy-Item $built $dist -Force -ErrorAction SilentlyContinue

# Verify the copy landed rather than trusting it. Windows locks a running executable, so
# if a Claude Code session currently has this MCP server open from dist\ the copy fails --
# and because $ErrorActionPreference is 'Continue' here (see above), that failure would
# otherwise be silent and leave a stale binary in place.
$builtHash = (Get-FileHash $built -Algorithm SHA256).Hash
$distHash = if (Test-Path $dist) { (Get-FileHash $dist -Algorithm SHA256).Hash } else { '' }
if ($builtHash -ne $distHash) {
    Write-Host ''
    Write-Host "Could not stage $dist" -ForegroundColor Red
    $holders = Get-Process -ErrorAction SilentlyContinue |
        Where-Object { $_.Path -eq $dist } |
        ForEach-Object { "PID $($_.Id) (started $($_.StartTime))" }
    if ($holders) {
        Write-Host "It is locked by a running process:" -ForegroundColor Yellow
        $holders | ForEach-Object { Write-Host "  $_" -ForegroundColor Yellow }
        Write-Host "That is this MCP server. Disconnect it in Claude Code (or stop the process)" -ForegroundColor Yellow
        Write-Host "and run this script again." -ForegroundColor Yellow
    }
    throw "dist\codex-imagegen.exe does not match the build output"
}

# The binary gets published, so verify the remapping actually took rather than trusting
# that the flags reached rustc.
$leaked = Select-String -Path $dist -Pattern ([regex]::Escape($env:USERPROFILE)) -SimpleMatch -Quiet -ErrorAction SilentlyContinue
if ($leaked) {
    throw "$dist still contains $env:USERPROFILE - path remapping did not take effect"
}

$size = [math]::Round((Get-Item $dist).Length / 1KB)
Write-Host ''
Write-Host "Staged $dist ($size KB)" -ForegroundColor Green
& $dist --version
Write-Host ''
Write-Host 'dist\ is gitignored. A registered MCP server locks its exe, so register a copy kept'
Write-Host 'outside the repository rather than dist\ itself, for example:'
Write-Host '  copy dist\codex-imagegen.exe C:\tools\'
Write-Host '  claude mcp add --scope user codex-imagegen -- C:\tools\codex-imagegen.exe'
