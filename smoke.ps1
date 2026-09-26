<#
.SYNOPSIS
  End-to-end check of dist\codex-imagegen.exe against the real Codex CLI.

.DESCRIPTION
  Drives the staged server over MCP stdio, as Claude Code does: one request at a time, each
  response matched by its id, lines read with a timeout.

  Without -SpendQuota it runs only the free steps: initialize, tools/list and status (status
  starts Codex and makes free calls only), then closes the server's stdin and checks that no Codex
  process of ours is left. Nothing is generated and no quota is spent.

  With -SpendQuota it also generates two images, about 2 images of the ChatGPT plan's image quota
  (docs/design.md, "smoke.ps1"; this is the milestone M2 version):
    a) generate with a prompt holding double quotes, a backslash, a newline and non-ASCII text
       (V2: Codex must use it verbatim);
    b) generate again with the image from (a) as a reference image (V3);
  then reads Codex's rollout trace for the tools the agent model was offered (V1) and for the
  image tool's referenced_image_paths (V3), and the server's stderr for the savedPath Codex
  reported for each image (V1).

  With -CheckTrace it runs only the V1 and V3 trace checks, against the trace folder of an earlier
  run (its "trace" folder, or one trace-* folder in it). It starts nothing and spends nothing.

  Run it after .\build.ps1, when a change touches the protocol, spawning or turn handling. Tell
  the owner the cost before running it with -SpendQuota.

.EXAMPLE
  .\smoke.ps1 -CheckTrace $env:TEMP\codex-imagegen-smoke-<stamp>\trace -ReferencePath $env:TEMP\codex-imagegen-smoke-<stamp>\images\smoke-<stamp>-v1.png
#>
[CmdletBinding()]
param(
    # Required to generate images. Without it only the free steps run.
    [switch]$SpendQuota,
    # The server to test. Default: dist\codex-imagegen.exe next to this script.
    [string]$Exe,
    # Passed to the server as --codex-bin when given.
    [string]$CodexBin,
    # Offline: run only the V1 and V3 trace checks against this existing trace folder.
    [string]$CheckTrace,
    # With -CheckTrace: the reference image V3 looks for in referenced_image_paths.
    [string]$ReferencePath
)

# Not 'Stop': failures are collected as checks and summarised at the end.
$ErrorActionPreference = 'Continue'
Set-StrictMode -Version 2.0

# Resolved here rather than as the parameter's default: Windows PowerShell 5.1 leaves
# $PSScriptRoot empty while it binds parameters.
if (-not $Exe) { $Exe = Join-Path $PSScriptRoot 'dist\codex-imagegen.exe' }

$utf8 = New-Object System.Text.UTF8Encoding($false)

$script:checks = New-Object System.Collections.Generic.List[object]
function Add-Check([string]$Name, [string]$Outcome, [string]$Detail = '') {
    $script:checks.Add([pscustomobject]@{ Name = $Name; Outcome = $Outcome; Detail = $Detail })
    $color = switch ($Outcome) { 'PASS' { 'Green' } 'FAIL' { 'Red' } default { 'Yellow' } }
    $line = "  [$Outcome] $Name"
    if ($Detail) { $line += " - $Detail" }
    Write-Host $line -ForegroundColor $color
}
function Test-Check([string]$Name, [bool]$Ok, [string]$Detail = '') {
    Add-Check $Name $(if ($Ok) { 'PASS' } else { 'FAIL' }) $Detail
}

# The summary, and the exit code: 1 when any check failed.
function Complete-Smoke([string]$PassLabel, [string]$Footer = '', [string]$FailLabel = 'SMOKE FAIL') {
    $failed = @($script:checks | Where-Object { $_.Outcome -eq 'FAIL' })
    $manual = @($script:checks | Where-Object { $_.Outcome -eq 'MANUAL' })
    Write-Host ''
    Write-Host ("{0} checks: {1} passed, {2} failed, {3} to inspect by hand, {4} skipped" -f $script:checks.Count,
        @($script:checks | Where-Object { $_.Outcome -eq 'PASS' }).Count, $failed.Count, $manual.Count,
        @($script:checks | Where-Object { $_.Outcome -eq 'SKIP' }).Count)
    if ($failed.Count -gt 0) {
        Write-Host $FailLabel -ForegroundColor Red
        foreach ($f in $failed) { Write-Host "  $($f.Name): $($f.Detail)" -ForegroundColor Red }
        exit 1
    }
    Write-Host $PassLabel -ForegroundColor Green
    if ($Footer) { Write-Host $Footer }
    exit 0
}

# ---------------------------------------------------------------------------------------------
# Reading Codex's rollout trace (V1 and V3)
# ---------------------------------------------------------------------------------------------
#
# Each thread writes <root>\trace-<trace id>-<thread id>\ with trace.jsonl (one event per line)
# and payloads\<n>.json, which the events name by a path relative to that folder (codex-cli
# 0.156.0, verified on a real trace):
# - inference_started names the model request. A full request carries the tools in its first
#   input item, {type: "additional_tools", tools: [...]}, as namespaces {type: "namespace", name,
#   tools}; a follow-up request in the same turn has previous_response_id, carries only the new
#   input (the multi-MB image result among it) and no tools. The tools code-mode `exec` can call
#   are listed in exec's description as "### `name`" headings; the calls it shows as examples
#   are not.
# - tool_call_started with kind {type: "image_generation"} is an image call. Its
#   invocation_payload holds {tool_name, tool_namespace, payload: {type: "function", arguments:
#   "<JSON>"}}; the code_cell_started event of its runtime cell holds the JavaScript the agent
#   ran (source_js). summary.input_preview is truncated, so it is never used.
# - tool_call_ended and the code_cell_* responses name the tool results, which carry the image's
#   base64 (about 4 MB each). They are never read.
#
# JSON is parsed with JavaScriptSerializer rather than ConvertFrom-Json: it takes a multi-MB
# document in a fraction of a second, and a missing key reads as $null under Set-StrictMode.

Add-Type -AssemblyName System.Web.Extensions
$script:serializer = New-Object System.Web.Script.Serialization.JavaScriptSerializer
$script:serializer.MaxJsonLength = [int]::MaxValue
$script:serializer.RecursionLimit = 1000

# Parsed JSON: dictionaries and object arrays.
function ConvertFrom-JsonText([string]$Text) {
    return , $script:serializer.DeserializeObject($Text)
}

# The value at $Path (a list of keys) under $Object, or $null where any step is missing.
function Get-Field($Object, [string[]]$Path) {
    foreach ($key in $Path) {
        if ($Object -isnot [Collections.IDictionary]) { return $null }
        $Object = $Object[$key]
    }
    return , $Object
}

function Get-TraceLogs([string]$Root) {
    return @(Get-ChildItem -LiteralPath $Root -Recurse -Filter 'trace.jsonl' -File -ErrorAction SilentlyContinue | Sort-Object FullName)
}

# The trace events of the given types in one trace.jsonl, parsed, in order, one per pipeline
# item. Other lines are skipped unparsed.
function Get-TraceEvents($Log, [string[]]$Types) {
    $pattern = '"type"\s*:\s*"(' + ($Types -join '|') + ')"'
    foreach ($line in [IO.File]::ReadLines($Log.FullName, $utf8)) {
        if ($line -notmatch $pattern) { continue }
        $traceEvent = ConvertFrom-JsonText $line
        if ($Types -contains [string](Get-Field $traceEvent 'payload', 'type')) { $traceEvent }
    }
}

# One JavaScript string literal (quotes included) as the string it denotes.
function ConvertFrom-JsString([string]$Literal) {
    $body = $Literal.Substring(1, $Literal.Length - 2)
    $decode = [Text.RegularExpressions.MatchEvaluator] {
        param($m)
        $e = $m.Groups[1].Value
        if ($e.StartsWith('u{')) { return [char]::ConvertFromUtf32([Convert]::ToInt32($e.Substring(2, $e.Length - 3), 16)) }
        if ($e.Length -eq 5 -and $e.StartsWith('u')) { return [string][char][Convert]::ToInt32($e.Substring(1), 16) }
        if ($e.Length -eq 3 -and $e.StartsWith('x')) { return [string][char][Convert]::ToInt32($e.Substring(1), 16) }
        switch -CaseSensitive ($e) {
            'n' { return "`n" }
            'r' { return "`r" }
            't' { return "`t" }
            'b' { return [string][char]8 }
            'f' { return [string][char]12 }
            'v' { return [string][char]11 }
            '0' { return [string][char]0 }
            "`r`n" { return '' }
            "`n" { return '' }
            "`r" { return '' }
        }
        return $e
    }
    return [regex]::Replace($body, '\\(u\{[0-9A-Fa-f]+\}|u[0-9A-Fa-f]{4}|x[0-9A-Fa-f]{2}|\r\n|[\s\S])', $decode)
}

# The referenced_image_paths the agent's JavaScript passed, from every literal list in it.
# Literal is $false when the source names referenced_image_paths without a literal list.
function Get-JsReferencedPaths([string]$Source) {
    $string = '"(?:[^"\\]|\\[\s\S])*"|''(?:[^''\\]|\\[\s\S])*''|`(?:[^`\\$]|\\[\s\S]|\$(?!\{))*`'
    $list = '["'']?\breferenced_image_paths["'']?\s*:\s*\[\s*((?:(?:' + $string + ')\s*,\s*)*(?:' + $string + '))?\s*,?\s*\]'
    $paths = @()
    $lists = [regex]::Matches($Source, $list)
    foreach ($m in $lists) {
        foreach ($s in [regex]::Matches($m.Groups[1].Value, $string)) { $paths += ConvertFrom-JsString $s.Value }
    }
    $named = [regex]::Matches($Source, '\breferenced_image_paths\b').Count
    return [pscustomobject]@{ Paths = $paths; Literal = ($named -eq $lists.Count) }
}

# Every tool offered to the agent model, across every model request in every trace folder under
# $Root: top-level and namespaced tools, and the tools nested in code-mode exec.
function Read-OfferedTools([string]$Root) {
    $result = [pscustomobject]@{ Requests = 0; WithTools = 0; Incremental = 0; Tools = [ordered]@{}; Problems = @() }
    foreach ($log in Get-TraceLogs $Root) {
        foreach ($traceEvent in Get-TraceEvents $log @('inference_started')) {
            $relative = Get-Field $traceEvent 'payload', 'request_payload', 'path'
            if (-not $relative) { $result.Problems += "an inference_started event in $($log.Directory.Name) names no request payload"; continue }
            $file = Join-Path $log.DirectoryName $relative
            if (-not (Test-Path -LiteralPath $file)) { $result.Problems += "missing $file"; continue }
            $result.Requests++
            # A follow-up request is mostly the image's base64 and carries no tools: it is not
            # parsed.
            $text = [IO.File]::ReadAllText($file, $utf8)
            if ($text.IndexOf('"additional_tools"', [StringComparison]::Ordinal) -lt 0 -and
                $text.IndexOf('"tools"', [StringComparison]::Ordinal) -lt 0) {
                $result.Incremental++
                continue
            }
            $request = ConvertFrom-JsonText $text
            $text = $null
            $lists = @()
            $top = Get-Field $request 'tools'
            if ($top -is [Array]) { $lists += , $top }
            $items = Get-Field $request 'input'
            if ($items -is [Array]) {
                foreach ($item in $items) {
                    $tools = Get-Field $item 'tools'
                    if ((Get-Field $item 'type') -eq 'additional_tools' -and $tools -is [Array]) { $lists += , $tools }
                }
            }
            if ($lists.Count -eq 0) { $result.Incremental++; continue }
            $result.WithTools++
            foreach ($list in $lists) {
                foreach ($tool in $list) {
                    if ((Get-Field $tool 'type') -eq 'namespace') {
                        $inner = Get-Field $tool 'tools'
                        if ($inner -is [Array]) {
                            foreach ($t in $inner) { Add-OfferedTool $result ([string](Get-Field $tool 'name')) $t }
                        }
                    }
                    else {
                        Add-OfferedTool $result '' $tool
                    }
                }
            }
        }
    }
    return $result
}

function Add-OfferedTool($Result, [string]$Namespace, $Tool) {
    $name = [string](Get-Field $Tool 'name')
    # A hosted tool such as {type: "web_search"} has no name.
    if (-not $name) { $name = [string](Get-Field $Tool 'type') }
    $key = if ($Namespace) { "$Namespace.$name" } else { $name }
    if (-not $Result.Tools.Contains($key)) {
        $Result.Tools[$key] = [pscustomobject]@{ Key = $key; Namespace = $Namespace; Name = $name; Nested = $false }
    }
    $description = Get-Field $Tool 'description'
    if ($name -eq 'exec' -and $description -is [string]) {
        foreach ($m in [regex]::Matches($description, '(?m)^###\s+`([^`]+)`')) {
            $nestedName = $m.Groups[1].Value
            $nestedKey = "exec > $nestedName"
            if (-not $Result.Tools.Contains($nestedKey)) {
                # A nested tool is named <namespace>__<name>, as in image_gen__imagegen.
                $nestedNs = if ($nestedName -match '^(.+)__(.+)$') { $Matches[1] } else { '' }
                $Result.Tools[$nestedKey] = [pscustomobject]@{ Key = $nestedKey; Namespace = $nestedNs; Name = $nestedName; Nested = $true }
            }
        }
    }
}

# What V1 makes of one offered tool: Required, Allowed (with the reason it is documented as
# harmless), Forbidden (with what it is), or Unclassified.
function Get-ToolVerdict($Tool) {
    $name = $Tool.Name
    $bare = if ($Tool.Nested -and $name -match '^(.+)__(.+)$') { $Matches[2] } else { $name }
    if (-not $Tool.Nested -and $name -eq 'exec') { return @('Required', 'code mode') }
    if ($Tool.Nested -and $name -eq 'image_gen__imagegen') { return @('Required', 'the image tool') }
    if ($Tool.Namespace -match '^(collaboration|multi_agent.*|agents?)$') { return @('Forbidden', 'sub-agent') }
    if ($Tool.Namespace -match '^mcp(__|$)' -or $name -match '^mcp__') { return @('Forbidden', 'MCP server tool') }
    if ($Tool.Nested) {
        switch ($name) {
            'apply_patch' { return @('Allowed', 'offered to every model while an environment exists; under the read-only sandbox and approvalPolicy never, Codex rejects every patch') }
            'view_image' { return @('Allowed', 'reads an image into the model''s context; writes nothing') }
            'clock__curr_time' { return @('Allowed', 'clock') }
        }
    }
    else {
        switch ($name) {
            'wait' { return @('Allowed', 'code mode: waits on a yielded exec cell') }
            'sleep' { return @('Allowed', 'clock') }
            'request_user_input_async' { return @('Allowed', 'offered by the model catalogue, with no config switch in codex-cli 0.156.0; it records an agent message and returns at once, never waiting for an answer') }
        }
    }
    $forbidden = '^(shell.*|.*_shell|exec_command|unified_exec.*|write_stdin|web_search.*|browser.*|computer.*|spawn_agent|send_message|send_input|wait_agent|list_agents|interrupt_agent|followup_task|resume_agent|close_agent|request_user_input.*|skill.*|tool_suggest.*|tool_search.*|request_plugin_install|list_available_plugins)$'
    if ($bare -match $forbidden -or $name -match $forbidden) { return @('Forbidden', 'shell, stdin, web, browser, computer-use, sub-agent, user-input, skill or tool-suggest tool') }
    return @('Unclassified', '')
}

# Every image call in every trace folder under $Root, with the referenced_image_paths it was
# given: from its invocation payload when the trace has one, else from the JavaScript of the exec
# cell that made it.
function Read-ImageCalls([string]$Root) {
    $calls = New-Object System.Collections.Generic.List[object]
    foreach ($log in Get-TraceLogs $Root) {
        $cells = @{}
        foreach ($traceEvent in Get-TraceEvents $log @('code_cell_started', 'tool_call_started')) {
            $payload = Get-Field $traceEvent 'payload'
            if ((Get-Field $payload 'type') -eq 'code_cell_started') {
                $cells[[string](Get-Field $payload 'runtime_cell_id')] = [string](Get-Field $payload 'source_js')
                continue
            }
            if ((Get-Field $payload 'kind', 'type') -ne 'image_generation') { continue }
            $call = [pscustomobject]@{ Folder = $log.Directory.Name; At = Get-Field $traceEvent 'wall_time_unix_ms'; Source = ''; Paths = @(); Known = $false }
            $relative = Get-Field $payload 'invocation_payload', 'path'
            if ($relative) {
                $file = Join-Path $log.DirectoryName $relative
                if (Test-Path -LiteralPath $file) {
                    $invocation = ConvertFrom-JsonText ([IO.File]::ReadAllText($file, $utf8))
                    $arguments = Get-Field $invocation 'payload', 'arguments'
                    if ($arguments -is [string]) {
                        $refs = Get-Field (ConvertFrom-JsonText $arguments) 'referenced_image_paths'
                        $call.Paths = @($refs | Where-Object { $_ } | ForEach-Object { [string]$_ })
                        $call.Source = 'invocation payload'
                        $call.Known = $true
                    }
                }
            }
            if (-not $call.Known) {
                $cell = [string](Get-Field $payload 'requester', 'runtime_cell_id')
                if ($cells.ContainsKey($cell)) {
                    $found = Get-JsReferencedPaths $cells[$cell]
                    $call.Paths = @($found.Paths)
                    $call.Source = 'exec cell source'
                    $call.Known = $found.Literal
                }
            }
            $calls.Add($call)
        }
    }
    return $calls.ToArray()
}

# Resolved against PowerShell's location, not .NET's current directory: Windows PowerShell 5.1 does
# not move the latter on Set-Location, so [IO.Path]::GetFullPath would resolve a relative path
# against wherever the session started.
function Get-FullPathOrSelf([string]$Path) {
    try { return $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($Path) }
    catch { return $Path }
}

# V1 and V3 from the trace under $TraceRoot. V3 looks for $Reference, when given, in the image
# calls' referenced_image_paths. The lists are also written to $ReportPath, when given.
function Invoke-TraceChecks([string]$TraceRoot, [string]$Reference, [string]$ReportPath) {
    Write-Host '-> V1: tools offered to the agent model (rollout trace)'
    $offered = $null
    $calls = @()
    $traceError = $null
    $callsError = $null
    try {
        $offered = Read-OfferedTools $TraceRoot
    }
    catch {
        $traceError = $_.Exception.Message
    }
    if ($traceError -or $null -eq $offered) {
        Add-Check 'V1 and V3: the rollout trace is readable' 'MANUAL' "$traceError; inspect $TraceRoot by hand"
        return
    }
    # Separately, so a V3 parse problem cannot hide the V1 result.
    try {
        $calls = @(Read-ImageCalls $TraceRoot)
    }
    catch {
        $callsError = $_.Exception.Message
    }

    $byVerdict = @{ Required = @(); Allowed = @(); Forbidden = @(); Unclassified = @() }
    foreach ($tool in $offered.Tools.Values) {
        $verdict = Get-ToolVerdict $tool
        $shown = if ($verdict[0] -eq 'Allowed') { "$($tool.Key) ($($verdict[1]))" } else { $tool.Key }
        $byVerdict[$verdict[0]] += $shown
    }
    $top = @($offered.Tools.Values | Where-Object { -not $_.Nested } | ForEach-Object { $_.Key })
    $nested = @($offered.Tools.Values | Where-Object { $_.Nested } | ForEach-Object { $_.Name })
    $none = { param($list) if (@($list).Count) { @($list) -join ', ' } else { 'none' } }
    $lines = @(
        "model requests: $($offered.Requests) ($($offered.WithTools) with a tool list, $($offered.Incremental) follow-ups without one)",
        "offered: $(& $none $top)",
        "nested in exec: $(& $none $nested)",
        "allowed (documented): $(& $none $byVerdict.Allowed)",
        "forbidden: $(& $none $byVerdict.Forbidden)",
        "unclassified: $(& $none $byVerdict.Unclassified)",
        "image calls: $($calls.Count)"
    )
    foreach ($call in $calls) {
        $paths = if ($call.Known) { "[$(@($call.Paths) -join ', ')]" } else { 'unknown (not a literal list)' }
        $lines += "  $($call.Folder): referenced_image_paths $paths, from the $(if ($call.Source) { $call.Source } else { 'trace: none found' })"
    }
    foreach ($p in $offered.Problems) { $lines += "problem: $p" }
    foreach ($l in $lines) { Write-Host "      $l" -ForegroundColor DarkGray }
    if ($ReportPath) {
        [IO.File]::WriteAllLines($ReportPath, [string[]]$lines, $utf8)
        Write-Host "      saved to $ReportPath" -ForegroundColor DarkGray
    }

    if ($offered.WithTools -eq 0) {
        Add-Check 'V1: tool list' 'MANUAL' "no model request with a tool list found under $TraceRoot; inspect it by hand"
    }
    else {
        Test-Check 'V1: exec is offered' (@($offered.Tools.Values | Where-Object { -not $_.Nested -and $_.Name -eq 'exec' }).Count -gt 0) ''
        Test-Check 'V1: image_gen__imagegen is nested in exec' ($nested -contains 'image_gen__imagegen') ''
        Test-Check 'V1: no shell, stdin, web search, browser, computer-use, sub-agent, user-input, skill, tool-suggest or MCP tool' ($byVerdict.Forbidden.Count -eq 0) (& $none $byVerdict.Forbidden)
        # The tool surface is a security boundary, so a tool nobody has classified fails the check
        # rather than waiting for someone to read a MANUAL line: a Codex update that adds a tool
        # should stop the run until the tool is judged and listed.
        Test-Check 'V1: every offered tool is classified' ($byVerdict.Unclassified.Count -eq 0) "unclassified: $(& $none $byVerdict.Unclassified); classify them in Get-ToolVerdict"
    }

    Write-Host '-> V3: the reference image in the image call (rollout trace)'
    $v3Name = 'V3: the reference reached the image tool''s referenced_image_paths'
    if (-not $Reference) {
        Add-Check $v3Name 'SKIP' 'no reference image to look for'
        return
    }
    if ($callsError) {
        Add-Check $v3Name 'MANUAL' "the image calls could not be read ($callsError); inspect $TraceRoot by hand"
        return
    }
    # Only a call made after the reference existed can name it. Text Codex sent to the model
    # never counts: the developer instructions, the tool's declaration and the tagged input name
    # both the parameter and the path whatever the agent does.
    $wanted = Get-FullPathOrSelf $Reference
    $hits = @($calls | Where-Object { @($_.Paths | ForEach-Object { Get-FullPathOrSelf $_ }) -contains $wanted })
    if ($hits.Count -gt 0) {
        Add-Check $v3Name 'PASS' "$wanted (from the $($hits[0].Source) of the image call in $($hits[0].Folder))"
    }
    elseif ($calls.Count -gt 0 -and @($calls | Where-Object { -not $_.Known }).Count -eq 0) {
        Test-Check $v3Name $false "no image call listed $wanted; see the image calls above"
    }
    else {
        Add-Check $v3Name 'MANUAL' "$($calls.Count) image call(s) found, not every one readable; inspect $TraceRoot by hand"
    }
}

# ---------------------------------------------------------------------------------------------
# Offline mode: the trace checks only
# ---------------------------------------------------------------------------------------------

if ($CheckTrace) {
    if ($SpendQuota) {
        Write-Host '-CheckTrace reads an existing trace and cannot be combined with -SpendQuota.' -ForegroundColor Red
        exit 1
    }
    Write-Host ''
    Write-Host 'codex-imagegen smoke test: offline trace check. Nothing is started and nothing is spent.'
    Write-Host "Trace: $CheckTrace"
    if ($ReferencePath) { Write-Host "Reference image: $ReferencePath" }
    Write-Host ''
    if (-not (Test-Path -LiteralPath $CheckTrace -PathType Container)) {
        Write-Host "Not a folder: $CheckTrace" -ForegroundColor Red
        exit 1
    }
    if (@(Get-TraceLogs $CheckTrace).Count -eq 0) {
        Write-Host "No trace.jsonl under $CheckTrace" -ForegroundColor Red
        exit 1
    }
    Invoke-TraceChecks $CheckTrace $ReferencePath ''
    Complete-Smoke 'TRACE CHECK PASS' '' 'TRACE CHECK FAIL'
}

# ---------------------------------------------------------------------------------------------
# The live run
# ---------------------------------------------------------------------------------------------

$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$work = Join-Path ([IO.Path]::GetTempPath()) "codex-imagegen-smoke-$stamp"
New-Item -ItemType Directory -Path $work -Force | Out-Null
$outDir = Join-Path $work 'images'
$traceRoot = Join-Path $work 'trace'

Write-Host ''
if ($SpendQuota) {
    Write-Host 'codex-imagegen smoke test: PAID run.' -ForegroundColor Yellow
    Write-Host 'Expected cost: about 2 images of the ChatGPT plan''s image quota, plus the agent' -ForegroundColor Yellow
    Write-Host 'model''s tokens for two short turns.' -ForegroundColor Yellow
}
else {
    Write-Host 'codex-imagegen smoke test: free steps only (initialize, tools/list, status).'
    Write-Host 'Pass -SpendQuota to also generate images (about 2 images of the image quota).'
}
Write-Host "Working folder: $work"
Write-Host ''

if (-not (Test-Path -LiteralPath $Exe)) {
    Write-Host "Server not found: $Exe. Run .\build.ps1 first." -ForegroundColor Red
    exit 1
}

# ---------------------------------------------------------------------------------------------
# The server process and a minimal MCP client
# ---------------------------------------------------------------------------------------------

$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = (Resolve-Path -LiteralPath $Exe).Path
if ($CodexBin) { $psi.Arguments = '--codex-bin "' + $CodexBin + '"' }
$psi.UseShellExecute = $false
$psi.RedirectStandardInput = $true
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $true
$psi.CreateNoWindow = $true
$psi.StandardOutputEncoding = $utf8
$psi.StandardErrorEncoding = $utf8
if ($SpendQuota) {
    # Inherited by the Codex child: it writes a rollout trace bundle per thread (V1).
    New-Item -ItemType Directory -Path $traceRoot -Force | Out-Null
    $psi.EnvironmentVariables['CODEX_ROLLOUT_TRACE_ROOT'] = $traceRoot
}

$proc = [System.Diagnostics.Process]::Start($psi)
# Our own writer over the pipe: UTF-8 without a byte-order mark, which Windows PowerShell's
# default stdin encoding would not guarantee.
$script:stdin = New-Object System.IO.StreamWriter($proc.StandardInput.BaseStream, $utf8)
$script:stdout = $proc.StandardOutput
# Drained continuously, so the server never blocks on a full stderr pipe; saved at the end.
$stderrTask = $proc.StandardError.ReadToEndAsync()
$script:pendingRead = $null
$script:eof = $false
$script:nextId = 0

# One line from the server, or $null after $TimeoutMs. A read that times out stays pending and is
# picked up by the next call, so no line is ever lost.
function Read-ServerLine([int]$TimeoutMs) {
    if ($null -eq $script:pendingRead) { $script:pendingRead = $script:stdout.ReadLineAsync() }
    if (-not $script:pendingRead.Wait([Math]::Max(1, $TimeoutMs))) { return $null }
    $line = $script:pendingRead.Result
    $script:pendingRead = $null
    if ($null -eq $line) { $script:eof = $true }
    return $line
}

# Send one request and wait for its response. Notifications in between (progress) are shown.
# Throws on a timeout, a closed stream, or a response to some other id.
function Invoke-Mcp([string]$Method, $Params, [int]$TimeoutSec, [switch]$WithProgress) {
    $script:nextId++
    $id = $script:nextId
    $message = [ordered]@{ jsonrpc = '2.0'; id = $id; method = $Method }
    if ($null -ne $Params) {
        if ($WithProgress) { $Params['_meta'] = @{ progressToken = $id } }
        $message['params'] = $Params
    }
    $line = ConvertTo-Json -InputObject $message -Depth 20 -Compress
    $script:stdin.Write($line + "`n")
    $script:stdin.Flush()
    $deadline = (Get-Date).AddSeconds($TimeoutSec)
    while ($true) {
        $left = [int]($deadline - (Get-Date)).TotalMilliseconds
        if ($left -le 0) { throw "no response to $Method (id $id) within $TimeoutSec s" }
        $text = Read-ServerLine $left
        if ($null -eq $text) {
            if ($script:eof) { throw "the server closed its output before answering $Method" }
            continue
        }
        if ($text.Trim() -eq '') { continue }
        $obj = ConvertFrom-Json -InputObject $text
        $names = $obj.PSObject.Properties.Name
        if (($names -contains 'id') -and ($null -ne $obj.id)) {
            if ([string]$obj.id -ne [string]$id) { throw "got a response to id $($obj.id) while waiting for $id" }
            if ($names -contains 'error') { throw "$Method failed: $($obj.error.message)" }
            return $obj.result
        }
        if ($obj.method -eq 'notifications/progress') {
            Write-Host "      progress: $($obj.params.message)" -ForegroundColor DarkGray
        }
        else {
            Write-Host "      notification: $($obj.method)" -ForegroundColor DarkGray
        }
    }
}

function Get-ResultText($Result) {
    $texts = @($Result.content | Where-Object { $_.type -eq 'text' } | ForEach-Object { $_.text })
    return ($texts -join "`n")
}

# Width and height from a JPEG's SOF marker, or $null.
function Get-JpegSize([byte[]]$Bytes) {
    if ($Bytes.Length -lt 4 -or $Bytes[0] -ne 0xFF -or $Bytes[1] -ne 0xD8) { return $null }
    $i = 2
    while ($i + 9 -lt $Bytes.Length) {
        if ($Bytes[$i] -ne 0xFF) { return $null }
        $marker = [int]$Bytes[$i + 1]
        if ($marker -eq 0xFF) { $i++; continue }
        $length = ([int]$Bytes[$i + 2] -shl 8) -bor [int]$Bytes[$i + 3]
        $isSof = ($marker -ge 0xC0 -and $marker -le 0xCF) -and ($marker -notin 0xC4, 0xC8, 0xCC)
        if ($isSof) {
            $height = ([int]$Bytes[$i + 5] -shl 8) -bor [int]$Bytes[$i + 6]
            $width = ([int]$Bytes[$i + 7] -shl 8) -bor [int]$Bytes[$i + 8]
            return [pscustomobject]@{ Width = $width; Height = $height }
        }
        $i += 2 + $length
    }
    return $null
}

# Every process under $RootPid, recorded with its start time so a recycled process id is not
# mistaken for a survivor.
function Get-Descendants([int]$RootPid) {
    $all = @(Get-CimInstance Win32_Process | Select-Object ProcessId, ParentProcessId, Name, CreationDate)
    $found = @()
    $queue = New-Object System.Collections.Generic.Queue[int]
    $queue.Enqueue($RootPid)
    while ($queue.Count -gt 0) {
        $parent = $queue.Dequeue()
        foreach ($p in $all | Where-Object { $_.ParentProcessId -eq $parent }) {
            $found += $p
            $queue.Enqueue([int]$p.ProcessId)
        }
    }
    return $found
}

# Check one generate result against the design (docs/design.md, "Success result").
function Test-GenerateResult([string]$Label, $Result, [string]$Session, [string]$Prompt) {
    $head = ((Get-ResultText $Result) -split "`n" | Select-Object -First 3) -join ' | '
    Test-Check "$Label is not an error" ($Result.isError -ne $true) $head
    $images = @($Result.content | Where-Object { $_.type -eq 'image' })
    Test-Check "$Label has exactly one image block" ($images.Count -eq 1) "$($images.Count) image block(s)"
    if ($images.Count -ge 1) {
        $image = $images[0]
        Test-Check "$Label preview is image/jpeg" ($image.mimeType -eq 'image/jpeg') $image.mimeType
        $bytes = [Convert]::FromBase64String($image.data)
        Test-Check "$Label preview is at most 512,000 bytes" ($bytes.Length -le 512000) "$($bytes.Length) bytes"
        $size = Get-JpegSize $bytes
        if ($null -eq $size) {
            Test-Check "$Label preview has a readable JPEG frame header" $false
        }
        else {
            $edge = [Math]::Max($size.Width, $size.Height)
            Test-Check "$Label preview long edge is at most 1024 px" ($edge -le 1024) "$($size.Width)x$($size.Height)"
        }
    }
    $text = Get-ResultText $Result
    $expected = Join-Path $outDir "$Session-v1.png"
    $named = $text -match ('(?m)^image: ' + [regex]::Escape($expected) + '  \(')
    Test-Check "$Label names $Session-v1.png in the output folder" $named
    Test-Check "$Label file exists" (Test-Path -LiteralPath $expected) $expected
    # V2, compared here rather than taken from the server's own comparison: the prompt Codex says
    # it used, JSON-quoted on its line, must equal the prompt sent, whitespace at either end aside.
    $promptLines = @($text -split "`n" | Where-Object { $_ -match '^codex prompt: ' })
    if ($promptLines.Count -ne 1 -or $promptLines[0] -eq 'codex prompt: (not reported)') {
        Test-Check "$Label Codex reported the prompt it used (V2)" $false ($promptLines -join ' | ')
    }
    else {
        try {
            $reported = [string](ConvertFrom-Json -InputObject $promptLines[0].Substring('codex prompt: '.Length))
            Test-Check "$Label Codex used the prompt verbatim (V2)" ($reported.Trim() -ceq $Prompt.Trim()) $reported
        }
        catch {
            Test-Check "$Label Codex's prompt line is readable (V2)" $false $promptLines[0]
        }
    }
    $promptWarnings = @($text -split "`n" | Where-Object { $_ -match '^warning: (codex prompt differs|Codex did not report the prompt)' })
    Test-Check "$Label the server raised no prompt warning (V2)" ($promptWarnings.Count -eq 0) ($promptWarnings -join ' ')
    foreach ($warning in @($text -split "`n" | Where-Object { $_ -like 'warning:*' })) {
        Write-Host "      $warning" -ForegroundColor Yellow
    }
    return $expected
}

# ---------------------------------------------------------------------------------------------
# The run
# ---------------------------------------------------------------------------------------------

$codexPid = $null
$descendants = @()
$v1 = $null
# The sessions whose generate succeeded, each of which must have logged Codex's savedPath (V1).
$sessionsMade = @()
try {
    Write-Host '-> initialize'
    $init = Invoke-Mcp 'initialize' @{ protocolVersion = '2025-11-25'; capabilities = @{}; clientInfo = @{ name = 'codex-imagegen-smoke'; version = '0' } } 30
    Test-Check 'initialize negotiates 2025-11-25' ($init.protocolVersion -eq '2025-11-25') $init.protocolVersion
    $script:stdin.Write('{"jsonrpc":"2.0","method":"notifications/initialized"}' + "`n")
    $script:stdin.Flush()

    Write-Host '-> tools/list'
    $tools = Invoke-Mcp 'tools/list' @{} 30
    $names = @($tools.tools | ForEach-Object { $_.name })
    $wanted = @('codex_imagegen_generate', 'codex_imagegen_refine', 'codex_imagegen_status')
    Test-Check 'tools/list offers the three tools' (-not (Compare-Object $names $wanted)) ($names -join ', ')

    Write-Host '-> status (starts Codex; free calls only)'
    $status = Get-ResultText (Invoke-Mcp 'tools/call' @{ name = 'codex_imagegen_status'; arguments = @{} } 120)
    Write-Host ($status -replace '(?m)^', '      ') -ForegroundColor DarkGray
    Test-Check 'status: image generation is available' ($status -match '(?m)^image generation: available') ''
    if ($status -match 'app-server: running \(pid (\d+)\)') {
        $codexPid = [int]$Matches[1]
        $descendants = @(Get-Descendants $proc.Id)
        Test-Check 'the Codex app-server runs under the server' (@($descendants | Where-Object { $_.ProcessId -eq $codexPid }).Count -eq 1) "pid $codexPid"
    }

    if ($SpendQuota) {
        $e = [char]0x00E9
        $u = [char]0x00FC
        $dash = [char]0x2014
        $promptA = 'A small "brass" lighthouse on a rocky islet at dusk, loose watercolour.' + "`n" +
            'A signboard by its door reads C:\keeper ' + $dash + ' Caf' + $e + ' Br' + $u + 'cke.'
        $sessionA = "smoke-$stamp"
        Write-Host "-> generate $sessionA (1 image)"
        $started = Get-Date
        $resultA = Invoke-Mcp 'tools/call' @{ name = 'codex_imagegen_generate'; arguments = @{ prompt = $promptA; session = $sessionA; output_dir = $outDir } } 420 -WithProgress
        Write-Host ("      took {0:N1} s" -f ((Get-Date) - $started).TotalSeconds)
        Write-Host ((Get-ResultText $resultA) -replace '(?m)^', '      ') -ForegroundColor DarkGray
        $v1 = Test-GenerateResult 'generate' $resultA $sessionA $promptA
        if ($resultA.isError -ne $true) { $sessionsMade += $sessionA }

        $sessionB = "smoke-$stamp-ref"
        $promptB = 'The same lighthouse and islet as in the reference image, but in deep winter: snow on the rocks, frozen spray, pale morning light.'
        Write-Host "-> generate $sessionB with the first image as a reference (1 image)"
        if (Test-Path -LiteralPath $v1) {
            $started = Get-Date
            $resultB = Invoke-Mcp 'tools/call' @{ name = 'codex_imagegen_generate'; arguments = @{ prompt = $promptB; session = $sessionB; output_dir = $outDir; reference_images = @($v1) } } 420 -WithProgress
            Write-Host ("      took {0:N1} s" -f ((Get-Date) - $started).TotalSeconds)
            Write-Host ((Get-ResultText $resultB) -replace '(?m)^', '      ') -ForegroundColor DarkGray
            $v1ref = Test-GenerateResult 'generate with a reference' $resultB $sessionB $promptB
            if ($resultB.isError -ne $true) { $sessionsMade += $sessionB }
            Add-Check 'V3: the second image follows the reference' 'MANUAL' "compare $v1 with $v1ref"
        }
        else {
            Test-Check 'generate with a reference' $false 'skipped: the first image is missing'
        }

        Write-Host '-> status'
        $status2 = Get-ResultText (Invoke-Mcp 'tools/call' @{ name = 'codex_imagegen_status'; arguments = @{} } 120)
        Test-Check 'status: no turn is left running' ($status2 -match '(?m)^running turns: none') ''
        if ($codexPid) {
            $pidNow = if ($status2 -match 'app-server: running \(pid (\d+)\)') { [int]$Matches[1] } else { $null }
            Test-Check 'the Codex app-server ran the whole time, never respawned' ($pidNow -eq $codexPid) "pid $codexPid at the start, $(if ($pidNow) { "pid $pidNow" } else { 'not running' }) now"
        }
    }
}
catch {
    Test-Check 'the MCP conversation' $false $_.Exception.Message
}

# ---------------------------------------------------------------------------------------------
# Shutdown: closing stdin ends the server, and its job object takes the Codex tree with it
# ---------------------------------------------------------------------------------------------

# Every process of ours alive now is checked after the exit, not only those seen at the first
# status: a respawned app-server, or a helper Codex started during a turn, counts too. Taken while
# the server still runs, so the parent chain can still be walked; the union keeps a child that
# died and was replaced.
if (-not $proc.HasExited) {
    $descendants = @(@($descendants) + @(Get-Descendants $proc.Id) | Sort-Object ProcessId, CreationDate -Unique)
}

Write-Host '-> close stdin'
try { $script:stdin.Close() } catch { }
$exited = $proc.WaitForExit(20000)
Test-Check 'the server exits after its stdin closes' $exited ''
if (-not $exited) { try { $proc.Kill() } catch { } }
$serverStderr = $null
if ($stderrTask.Wait(5000)) {
    $serverStderr = $stderrTask.Result
    $logPath = Join-Path $work 'server-stderr.log'
    [IO.File]::WriteAllText($logPath, $serverStderr, $utf8)
    Write-Host "      server stderr saved to $logPath" -ForegroundColor DarkGray
}
Start-Sleep -Milliseconds 500
$survivors = @()
foreach ($p in $descendants) {
    $now = Get-CimInstance Win32_Process -Filter "ProcessId=$($p.ProcessId)" -ErrorAction SilentlyContinue
    if ($now -and $now.CreationDate -eq $p.CreationDate) { $survivors += "$($p.Name) (pid $($p.ProcessId))" }
}
if ($descendants.Count -gt 0) {
    Test-Check 'no Codex process of ours is left' ($survivors.Count -eq 0) ($survivors -join ', ')
}
else {
    Add-Check 'no Codex process of ours is left' 'SKIP' 'no child process was recorded'
}

# ---------------------------------------------------------------------------------------------
# V1 and V3, from Codex's rollout trace and the server's stderr
# ---------------------------------------------------------------------------------------------

if ($SpendQuota) {
    Invoke-TraceChecks $traceRoot $v1 (Join-Path $work 'v1-tools.txt')

    # V1's last part: Codex reports savedPath. The result reads the same when the server falls
    # back to the image's base64, so the server logs where each image came from (turn.rs,
    # image_source_line), and the copy it published must match Codex's file byte for byte.
    if ($null -eq $serverStderr) {
        Add-Check 'V1: Codex reported savedPath for each image' 'MANUAL' 'the server''s stderr was not captured'
    }
    else {
        $savedLines = @([regex]::Matches($serverStderr, '(?m)^codex-imagegen: session ([A-Za-z0-9._-]+): image item has savedPath (.+?)\r?$'))
        $fallbacks = @([regex]::Matches($serverStderr, '(?m)^codex-imagegen: session [A-Za-z0-9._-]+: image item has no savedPath.*$'))
        Test-Check 'V1: no image lacked savedPath (no base64 fallback)' ($fallbacks.Count -eq 0) (@($fallbacks | ForEach-Object { $_.Value.Trim() }) -join ' | ')
        $codexHome = if ($env:CODEX_HOME) { $env:CODEX_HOME } else { Join-Path $env:USERPROFILE '.codex' }
        $imagesRoot = [IO.Path]::GetFullPath((Join-Path $codexHome 'generated_images')).TrimEnd('\') + '\'
        foreach ($session in $sessionsMade) {
            $mine = @($savedLines | Where-Object { $_.Groups[1].Value -eq $session })
            if ($mine.Count -ne 1) {
                Test-Check "V1: Codex reported savedPath for $session" $false "$($mine.Count) savedPath line(s) in the server's stderr"
                continue
            }
            $savedPath = $mine[0].Groups[2].Value
            $copy = Join-Path $outDir "$session-v1.png"
            $same = $savedPath.StartsWith($imagesRoot, [StringComparison]::OrdinalIgnoreCase) -and
                (Test-Path -LiteralPath $savedPath) -and (Test-Path -LiteralPath $copy) -and
                ((Get-FileHash -LiteralPath $savedPath).Hash -eq (Get-FileHash -LiteralPath $copy).Hash)
            Test-Check "V1: Codex reported savedPath for $session, and $session-v1.png is a byte copy of it" $same $savedPath
        }
    }
    Write-Host "      the trace holds prompts and tool I/O; delete $traceRoot when done" -ForegroundColor DarkGray
}

# ---------------------------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------------------------

if ($SpendQuota) {
    Complete-Smoke 'SMOKE PASS' "Images: $outDir"
}
Complete-Smoke 'SMOKE PASS (free steps only)'
