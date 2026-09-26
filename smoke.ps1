<#
.SYNOPSIS
  End-to-end check of dist\codex-imagegen.exe against the real Codex CLI.

.DESCRIPTION
  Drives the staged server over MCP stdio, as Claude Code does: one request at a time, each
  response matched by its id, lines read with a timeout. Every server it starts runs with
  CODEX_IMAGEGEN_HOME pointing at a fresh state folder under the run's working folder, so the
  sessions it makes and removes are its own, never the user's.

  Without -SpendQuota it runs only the free steps: initialize, tools/list and status (status
  starts Codex and makes free calls only), then closes the server's stdin and checks that no Codex
  process of ours is left, then runs --cleanup on the empty state folder. Nothing is generated and
  no quota is spent.

  With -SpendQuota it also spends about 3 images of the ChatGPT plan's image quota (docs/design.md,
  "smoke.ps1"; this is the milestone M3 version):
    a) generate(session=smoke-<stamp>) with a prompt holding double quotes, a backslash, a newline
       and non-ASCII text (V2: Codex must use it verbatim);
    b) refine that session with feedback holding the same kinds of character (V2 for refine), and
       check in Codex's rollout trace that the image call's referenced_image_paths named the
       first image, the edit target (V3 for refine);
    c) kill the server, start a fresh one on the same state folder, and refine again (resume after
       a restart);
    d) status lists the session with 3 turns;
  then reads the trace for the tools the agent model was offered (V1) and the servers' stderr for
  the savedPath Codex reported for each image (V1), and finally, free:
    e) closes every server and, whatever the run got to, runs codex-imagegen.exe --cleanup
       --older-than-days 0 on the state folder. It must remove every session the store records:
       its record, the files the run published (<session>-v1..vN, which the record must list
       exactly), Codex's image folder for its thread, and the thread's rollout (which must be found
       before the sweep, and be gone from that exact path after it; a listing error fails). A
       recorded session the run lost track of fails the run, and is still removed.

  With -Interrupt as well (V4), (b) to (d) are replaced by one refine that is cancelled
  (notifications/cancelled) a few seconds after its image call starts: about 1 image plus 1 cut
  short in all. The cancelled request must get no response; the server must send turn/interrupt
  and Codex must complete the turn as interrupted; after the time an image takes, no file may have
  been published or saved by Codex for it, and the session must still hold its one image.

  With -CodexHome <dir> (V5), every server runs with --codex-home <dir>. status must report that
  home, each session's record must name it, every image Codex saved must be under it, and nothing
  of the run's threads may be in an ambient home ($env:CODEX_HOME, and %USERPROFILE%\.codex). The
  home must be signed in already: $env:CODEX_HOME = '<dir>'; codex login --device-auth; then
  Remove-Item Env:CODEX_HOME, so the variable does not linger in the window that runs this script.

  With -Concurrent as well (V9, about 2 more images), two server processes on the same state
  folder, each with its own stdin, generate at the same moment; both must succeed, and both
  sessions must be recorded. Step (e) then removes them too.

  With -CheckTrace it runs only the V1 and V3 trace checks, against the trace folder of an earlier
  run (its "trace" folder, or one trace-* folder in it). It starts nothing and spends nothing.

  Run it after .\build.ps1, when a change touches the protocol, spawning or session handling. Tell
  the owner the cost before running it with -SpendQuota.

.EXAMPLE
  .\smoke.ps1 -CheckTrace $env:TEMP\codex-imagegen-smoke-<stamp>\trace -ReferencePath $env:TEMP\codex-imagegen-smoke-<stamp>\images\smoke-<stamp>-v1.png
#>
[CmdletBinding()]
param(
    # Required to generate images. Without it only the free steps run.
    [switch]$SpendQuota,
    # With -SpendQuota: also V9, two servers generating at the same moment (about 2 more images).
    [switch]$Concurrent,
    # With -SpendQuota: V4, a refine cancelled during its image call, instead of (b) to (d).
    [switch]$Interrupt,
    # Passed to every server as --codex-home (V5). Must be an existing, signed-in Codex home.
    [string]$CodexHome,
    # The server to test. Default: dist\codex-imagegen.exe next to this script.
    [string]$Exe,
    # Passed to the server as --codex-bin when given.
    [string]$CodexBin,
    # Offline: run only the V1 and V3 trace checks against this existing trace folder.
    [string]$CheckTrace,
    # With -CheckTrace: the image V3 looks for in referenced_image_paths. Several paths may be
    # given; any of them counts (a refine's edit target is Codex's copy or ours, byte-identical).
    [string[]]$ReferencePath
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
# $Root: top-level and namespaced tools, and the tools nested in code-mode exec. A request whose
# payload is missing or cannot be read is listed in Problems, which fails V1: the tools it
# carried were never checked.
function Read-OfferedTools([string]$Root) {
    $result = [pscustomobject]@{ Requests = 0; WithTools = 0; Incremental = 0; Tools = [ordered]@{}; Problems = @() }
    foreach ($log in Get-TraceLogs $Root) {
        foreach ($traceEvent in Get-TraceEvents $log @('inference_started')) {
            $relative = Get-Field $traceEvent 'payload', 'request_payload', 'path'
            if (-not $relative) { $result.Problems += "an inference_started event in $($log.Directory.Name) names no request payload"; continue }
            $file = Join-Path $log.DirectoryName $relative
            if (-not (Test-Path -LiteralPath $file)) { $result.Problems += "missing $file"; continue }
            $result.Requests++
            try {
                # A follow-up request is mostly the image's base64 and carries no tools: it is not
                # parsed.
                $text = [IO.File]::ReadAllText($file, $utf8)
                if ($text.IndexOf('"additional_tools"', [StringComparison]::Ordinal) -lt 0 -and
                    $text.IndexOf('"tools"', [StringComparison]::Ordinal) -lt 0) {
                    $result.Incremental++
                    continue
                }
                $request = ConvertFrom-JsonText $text
            }
            catch {
                # The parser's message quotes the document it failed on: only its first line.
                $why = ($_.Exception.Message -split '\r?\n')[0]
                if ($why.Length -gt 200) { $why = $why.Substring(0, 200) + '...' }
                $result.Problems += "unreadable ${file}: $why"
                continue
            }
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

# V1 and V3 from the trace under $TraceRoot. V3 looks for any of $References, when given, in the
# image calls' referenced_image_paths. The lists are also written to $ReportPath, when given.
function Invoke-TraceChecks([string]$TraceRoot, [string[]]$References, [string]$ReportPath,
    [string]$V3Name = 'V3: the reference reached the image tool''s referenced_image_paths') {
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

    # Checked first: the tool lists that were read say nothing about a request that was not, so
    # the tool-surface checks would pass on part of the evidence. They are not run then.
    $unread = @($offered.Problems)
    $readDetail = if ($unread.Count) { ($unread -join '; ') + '; the tool-surface checks were not run' } else { "$($offered.Requests) request(s)" }
    Test-Check 'V1: every model request''s payload is readable' ($unread.Count -eq 0) $readDetail
    if ($unread.Count -eq 0 -and $offered.WithTools -eq 0) {
        Add-Check 'V1: tool list' 'MANUAL' "no model request with a tool list found under $TraceRoot; inspect it by hand"
    }
    elseif ($unread.Count -eq 0) {
        Test-Check 'V1: exec is offered' (@($offered.Tools.Values | Where-Object { -not $_.Nested -and $_.Name -eq 'exec' }).Count -gt 0) ''
        Test-Check 'V1: image_gen__imagegen is nested in exec' ($nested -contains 'image_gen__imagegen') ''
        Test-Check 'V1: no shell, stdin, web search, browser, computer-use, sub-agent, user-input, skill, tool-suggest or MCP tool' ($byVerdict.Forbidden.Count -eq 0) (& $none $byVerdict.Forbidden)
        # The tool surface is a security boundary, so a tool nobody has classified fails the check
        # rather than waiting for someone to read a MANUAL line: a Codex update that adds a tool
        # should stop the run until the tool is judged and listed.
        Test-Check 'V1: every offered tool is classified' ($byVerdict.Unclassified.Count -eq 0) "unclassified: $(& $none $byVerdict.Unclassified); classify them in Get-ToolVerdict"
    }

    Write-Host '-> V3: the reference image in the image call (rollout trace)'
    $v3Name = $V3Name
    $given = @($References | Where-Object { $_ })
    if ($given.Count -eq 0) {
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
    # Any of the paths counts: a refine's edit target is Codex's copy or ours, byte-identical.
    $wanted = @($given | ForEach-Object { Get-FullPathOrSelf $_ })
    $hits = @($calls | Where-Object { @($_.Paths | ForEach-Object { Get-FullPathOrSelf $_ } | Where-Object { $wanted -contains $_ }).Count -gt 0 })
    if ($hits.Count -gt 0) {
        Add-Check $v3Name 'PASS' "$($wanted -join ' or ') (from the $($hits[0].Source) of the image call in $($hits[0].Folder))"
    }
    elseif ($calls.Count -gt 0 -and @($calls | Where-Object { -not $_.Known }).Count -eq 0) {
        Test-Check $v3Name $false "no image call listed $($wanted -join ' or '); see the image calls above"
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
    if ($ReferencePath) { Write-Host "Reference image: $($ReferencePath -join ' or ')" }
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

if ($Concurrent -and -not $SpendQuota) {
    Write-Host '-Concurrent generates images (V9) and needs -SpendQuota.' -ForegroundColor Red
    exit 1
}
if ($Interrupt -and -not $SpendQuota) {
    Write-Host '-Interrupt generates images (V4) and needs -SpendQuota.' -ForegroundColor Red
    exit 1
}
if ($CodexHome) {
    $CodexHome = (Get-FullPathOrSelf $CodexHome).TrimEnd('\')
    if (-not (Test-Path -LiteralPath $CodexHome -PathType Container)) {
        Write-Host "-CodexHome is not a folder: $CodexHome" -ForegroundColor Red
        exit 1
    }
}
# The home a server uses without --codex-home.
$ambientHome = if ($env:CODEX_HOME) { $env:CODEX_HOME } else { Join-Path $env:USERPROFILE '.codex' }
# V5's "nothing in the ambient home" looks in both: an inherited CODEX_HOME (left from the sign-in,
# say) may be the dedicated home itself, and the user's own home must stay untouched regardless.
$ambientHomes = @(@($env:CODEX_HOME, (Join-Path $env:USERPROFILE '.codex')) | Where-Object { $_ })

# Whether two spellings name the same folder, for the V5 checks: full paths, without a \\?\ prefix
# or a trailing backslash, compared case-insensitively.
function Test-SameFolder([string]$A, [string]$B) {
    $plain = { param($p) ((Get-FullPathOrSelf ($p -replace '^\\\\\?\\', '')) -replace '/', '\').TrimEnd('\') }
    return [string]::Equals((& $plain $A), (& $plain $B), [StringComparison]::OrdinalIgnoreCase)
}

# ---------------------------------------------------------------------------------------------
# The live run
# ---------------------------------------------------------------------------------------------

$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$work = Join-Path ([IO.Path]::GetTempPath()) "codex-imagegen-smoke-$stamp"
$outDir = Join-Path $work 'images'
$traceRoot = Join-Path $work 'trace'
# The servers' state base (CODEX_IMAGEGEN_HOME): fresh, so every session here is the run's own.
$stateBase = Join-Path $work 'state'
# The servers' working directory, which names the per-project state folder under the base.
$projectDir = Join-Path $work 'project'
foreach ($d in @($work, $outDir, $stateBase, $projectDir)) { New-Item -ItemType Directory -Path $d -Force | Out-Null }

Write-Host ''
if ($SpendQuota) {
    $images = if ($Interrupt) { 'about 1 image plus 1 cut short' } else { 'about 3 images' }
    if ($Concurrent) { $images += ', plus 2 for V9' }
    Write-Host 'codex-imagegen smoke test: PAID run.' -ForegroundColor Yellow
    Write-Host "Expected cost: $images of the ChatGPT plan's image quota, plus the agent" -ForegroundColor Yellow
    Write-Host 'model''s tokens for as many short turns.' -ForegroundColor Yellow
}
else {
    Write-Host 'codex-imagegen smoke test: free steps only (initialize, tools/list, status, --cleanup).'
    Write-Host 'Pass -SpendQuota to also generate and refine images (about 3 images of the image quota).'
}
Write-Host "Working folder: $work"
Write-Host "State base (CODEX_IMAGEGEN_HOME for every server here): $stateBase"
if ($CodexHome) { Write-Host "Codex home (--codex-home for every server here): $CodexHome" }
Write-Host ''

if (-not (Test-Path -LiteralPath $Exe)) {
    Write-Host "Server not found: $Exe. Run .\build.ps1 first." -ForegroundColor Red
    exit 1
}
$exePath = (Resolve-Path -LiteralPath $Exe).Path

# ---------------------------------------------------------------------------------------------
# Server processes and a minimal MCP client
# ---------------------------------------------------------------------------------------------

# How every codex-imagegen process here is started: the isolated state base, the run's own
# working directory, and no CLAUDE_PROJECT_DIR, so nothing lands in a real project.
function New-StartInfo([string]$Arguments) {
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $exePath
    $argv = @()
    if ($CodexBin) { $argv += '--codex-bin "' + $CodexBin + '"' }
    if ($CodexHome) { $argv += '--codex-home "' + $CodexHome + '"' }
    if ($Arguments) { $argv += $Arguments }
    $psi.Arguments = $argv -join ' '
    $psi.WorkingDirectory = $projectDir
    $psi.UseShellExecute = $false
    $psi.RedirectStandardInput = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.CreateNoWindow = $true
    $psi.StandardOutputEncoding = $utf8
    $psi.StandardErrorEncoding = $utf8
    $psi.EnvironmentVariables['CODEX_IMAGEGEN_HOME'] = $stateBase
    $psi.EnvironmentVariables.Remove('CLAUDE_PROJECT_DIR')
    if ($SpendQuota) {
        # Inherited by the Codex child: it writes a rollout trace bundle per thread (V1, V3).
        New-Item -ItemType Directory -Path $traceRoot -Force | Out-Null
        $psi.EnvironmentVariables['CODEX_ROLLOUT_TRACE_ROOT'] = $traceRoot
    }
    return $psi
}

function Start-Server([string]$Label) {
    $proc = [System.Diagnostics.Process]::Start((New-StartInfo ''))
    return [pscustomobject]@{
        Label       = $Label
        Proc        = $proc
        # Our own writer over the pipe: UTF-8 without a byte-order mark, which Windows
        # PowerShell's default stdin encoding would not guarantee.
        Stdin       = New-Object System.IO.StreamWriter($proc.StandardInput.BaseStream, $utf8)
        Stdout      = $proc.StandardOutput
        # Drained continuously, so the server never blocks on a full stderr pipe.
        StderrTask  = $proc.StandardError.ReadToEndAsync()
        Stderr      = $null
        PendingRead = $null
        Eof         = $false
        NextId      = 0
        Descendants = @()
    }
}

# One line from the server, or $null after $TimeoutMs. A read that times out stays pending and is
# picked up by the next call, so no line is ever lost.
function Read-ServerLine($Server, [int]$TimeoutMs) {
    if ($null -eq $Server.PendingRead) { $Server.PendingRead = $Server.Stdout.ReadLineAsync() }
    if (-not $Server.PendingRead.Wait([Math]::Max(1, $TimeoutMs))) { return $null }
    $line = $Server.PendingRead.Result
    $Server.PendingRead = $null
    if ($null -eq $line) { $Server.Eof = $true }
    return $line
}

# Send one request; its id.
function Send-Mcp($Server, [string]$Method, $Params, [switch]$WithProgress) {
    $Server.NextId++
    $id = $Server.NextId
    $message = [ordered]@{ jsonrpc = '2.0'; id = $id; method = $Method }
    if ($null -ne $Params) {
        if ($WithProgress) { $Params['_meta'] = @{ progressToken = $id } }
        $message['params'] = $Params
    }
    $Server.Stdin.Write((ConvertTo-Json -InputObject $message -Depth 20 -Compress) + "`n")
    $Server.Stdin.Flush()
    return $id
}

# Wait for the response to request $Id. Notifications in between (progress) are shown. Throws on a
# timeout, a closed stream, an error response, or a response to some other id.
function Receive-Mcp($Server, [int]$Id, [string]$Method, [int]$TimeoutSec) {
    $deadline = (Get-Date).AddSeconds($TimeoutSec)
    while ($true) {
        $left = [int]($deadline - (Get-Date)).TotalMilliseconds
        if ($left -le 0) { throw "$($Server.Label): no response to $Method (id $Id) within $TimeoutSec s" }
        $text = Read-ServerLine $Server $left
        if ($null -eq $text) {
            if ($Server.Eof) { throw "$($Server.Label): the server closed its output before answering $Method" }
            continue
        }
        if ($text.Trim() -eq '') { continue }
        $obj = ConvertFrom-Json -InputObject $text
        $names = $obj.PSObject.Properties.Name
        if (($names -contains 'id') -and ($null -ne $obj.id)) {
            if ([string]$obj.id -ne [string]$Id) { throw "$($Server.Label): got a response to id $($obj.id) while waiting for $Id" }
            if ($names -contains 'error') { throw "$($Server.Label): $Method failed: $($obj.error.message)" }
            return $obj.result
        }
        if ($obj.method -eq 'notifications/progress') {
            Write-Host "      [$($Server.Label)] progress: $($obj.params.message)" -ForegroundColor DarkGray
        }
        else {
            Write-Host "      [$($Server.Label)] notification: $($obj.method)" -ForegroundColor DarkGray
        }
    }
}

function Invoke-Mcp($Server, [string]$Method, $Params, [int]$TimeoutSec, [switch]$WithProgress) {
    $id = Send-Mcp $Server $Method $Params -WithProgress:$WithProgress
    return Receive-Mcp $Server $id $Method $TimeoutSec
}

function Initialize-Server($Server) {
    $init = Invoke-Mcp $Server 'initialize' @{ protocolVersion = '2025-11-25'; capabilities = @{}; clientInfo = @{ name = 'codex-imagegen-smoke'; version = '0' } } 30
    $Server.Stdin.Write('{"jsonrpc":"2.0","method":"notifications/initialized"}' + "`n")
    $Server.Stdin.Flush()
    return $init
}

function Get-ResultText($Result) {
    $texts = @($Result.content | Where-Object { $_.type -eq 'text' } | ForEach-Object { $_.text })
    return ($texts -join "`n")
}

function Invoke-Tool($Server, [string]$Tool, $Arguments, [int]$TimeoutSec = 420) {
    $started = Get-Date
    $result = Invoke-Mcp $Server 'tools/call' @{ name = $Tool; arguments = $Arguments } $TimeoutSec -WithProgress
    Write-Host ("      took {0:N1} s" -f ((Get-Date) - $started).TotalSeconds)
    Write-Host ((Get-ResultText $result) -replace '(?m)^', '      ') -ForegroundColor DarkGray
    return $result
}

function Get-Status($Server) {
    return Get-ResultText (Invoke-Mcp $Server 'tools/call' @{ name = 'codex_imagegen_status'; arguments = @{} } 120)
}

# V4: read the server's output until call $Id reports its image call started (the "generating
# image" progress phase). Response is $null then; it holds the call's response when the call
# answered first. Throws on a timeout or a closed stream.
function Wait-ImageStarted($Server, [int]$Id, [int]$TimeoutSec) {
    $deadline = (Get-Date).AddSeconds($TimeoutSec)
    while ($true) {
        $left = [int]($deadline - (Get-Date)).TotalMilliseconds
        if ($left -le 0) { throw "$($Server.Label): the image call did not start within $TimeoutSec s" }
        $text = Read-ServerLine $Server $left
        if ($null -eq $text) {
            if ($Server.Eof) { throw "$($Server.Label): the server closed its output" }
            continue
        }
        if ($text.Trim() -eq '') { continue }
        $obj = ConvertFrom-Json -InputObject $text
        $names = $obj.PSObject.Properties.Name
        if (($names -contains 'id') -and ($null -ne $obj.id)) {
            if ([string]$obj.id -ne [string]$Id) { throw "$($Server.Label): got a response to id $($obj.id) while waiting for $Id" }
            return [pscustomobject]@{ Response = $obj }
        }
        if ($obj.method -eq 'notifications/progress') {
            Write-Host "      [$($Server.Label)] progress: $($obj.params.message)" -ForegroundColor DarkGray
            if ([string]$obj.params.message -like 'generating image*') { return [pscustomobject]@{ Response = $null } }
        }
    }
}

# V4: read the server's output for $Seconds; every response to $Id seen meanwhile (a cancelled
# request must get none). Any other response throws; notifications are shown.
function Read-ResponsesTo($Server, [int]$Id, [int]$Seconds) {
    $deadline = (Get-Date).AddSeconds($Seconds)
    $responses = @()
    while ($true) {
        $left = [int]($deadline - (Get-Date)).TotalMilliseconds
        if ($left -le 0) { break }
        $text = Read-ServerLine $Server $left
        if ($null -eq $text) {
            if ($Server.Eof) { break }
            continue
        }
        if ($text.Trim() -eq '') { continue }
        $obj = ConvertFrom-Json -InputObject $text
        $names = $obj.PSObject.Properties.Name
        if (($names -contains 'id') -and ($null -ne $obj.id)) {
            if ([string]$obj.id -ne [string]$Id) { throw "$($Server.Label): got a response to id $($obj.id) while waiting for $Id" }
            $responses += $obj
            continue
        }
        Write-Host "      [$($Server.Label)] $(if ($obj.method -eq 'notifications/progress') { "progress: $($obj.params.message)" } else { "notification: $($obj.method)" })" -ForegroundColor DarkGray
    }
    return , $responses
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

# Remember every process of the server's alive now. Taken while the server still runs, so the
# parent chain can still be walked; the union keeps a child that died and was replaced.
function Watch-Descendants($Server) {
    if (-not $Server.Proc.HasExited) {
        $Server.Descendants = @(@($Server.Descendants) + @(Get-Descendants $Server.Proc.Id) | Sort-Object ProcessId, CreationDate -Unique)
    }
}

# Stop the server: close its stdin, or kill the exe outright (-Kill), which the job object must
# answer by taking the whole Codex tree with it. Either way no process of ours may be left.
function Stop-Server($Server, [switch]$Kill) {
    Watch-Descendants $Server
    if ($Kill) {
        Write-Host "-> kill $($Server.Label)"
        try { $Server.Proc.Kill() } catch { }
    }
    else {
        Write-Host "-> close $($Server.Label)'s stdin"
        try { $Server.Stdin.Close() } catch { }
    }
    $exited = $Server.Proc.WaitForExit(20000)
    Test-Check "$($Server.Label): the server exits$(if ($Kill) { ' when killed' } else { ' after its stdin closes' })" $exited ''
    if (-not $exited) { try { $Server.Proc.Kill() } catch { } }
    if ($Server.StderrTask.Wait(5000)) {
        $Server.Stderr = $Server.StderrTask.Result
        $name = ($Server.Label -replace '[^A-Za-z0-9]+', '-').Trim('-')
        $logPath = Join-Path $work "stderr-$name.log"
        [IO.File]::WriteAllText($logPath, $Server.Stderr, $utf8)
        Write-Host "      stderr saved to $logPath" -ForegroundColor DarkGray
    }
    Start-Sleep -Milliseconds 500
    $survivors = @()
    foreach ($p in $Server.Descendants) {
        $now = Get-CimInstance Win32_Process -Filter "ProcessId=$($p.ProcessId)" -ErrorAction SilentlyContinue
        if ($now -and $now.CreationDate -eq $p.CreationDate) { $survivors += "$($p.Name) (pid $($p.ProcessId))" }
    }
    if ($Server.Descendants.Count -gt 0) {
        Test-Check "$($Server.Label): no Codex process of ours is left" ($survivors.Count -eq 0) ($survivors -join ', ')
    }
    else {
        Add-Check "$($Server.Label): no Codex process of ours is left" 'SKIP' 'no child process was recorded'
    }
}

# codex-imagegen.exe --cleanup --older-than-days 0 on the state base: its exit code and output.
function Invoke-Cleanup {
    $proc = [System.Diagnostics.Process]::Start((New-StartInfo '--cleanup --older-than-days 0'))
    $proc.StandardInput.Close()
    $outTask = $proc.StandardOutput.ReadToEndAsync()
    $errTask = $proc.StandardError.ReadToEndAsync()
    $exited = $proc.WaitForExit(180000)
    if (-not $exited) { try { $proc.Kill() } catch { } }
    [void]$outTask.Wait(5000)
    [void]$errTask.Wait(5000)
    $result = [pscustomobject]@{
        Exited   = $exited
        ExitCode = $(if ($exited) { $proc.ExitCode } else { $null })
        Out      = $(if ($outTask.IsCompleted) { $outTask.Result } else { '' })
        Err      = $(if ($errTask.IsCompleted) { $errTask.Result } else { '' })
    }
    Write-Host ($result.Out -replace '(?m)^', '      ') -ForegroundColor DarkGray
    if ($result.Err.Trim()) { Write-Host ($result.Err -replace '(?m)^', '      ') -ForegroundColor DarkGray }
    return $result
}

# The project's session store as the servers here wrote it, parsed, or $null.
function Read-Store {
    $files = @(Get-ChildItem -LiteralPath $stateBase -Directory -ErrorAction SilentlyContinue |
        ForEach-Object { Join-Path $_.FullName 'sessions.json' } | Where-Object { Test-Path -LiteralPath $_ })
    if ($files.Count -ne 1) { return $null }
    return ConvertFrom-JsonText ([IO.File]::ReadAllText($files[0], $utf8))
}

# One session's record from a parsed store, or $null.
function Get-Record($Store, [string]$Session) {
    return Get-Field $Store 'sessions', $Session.ToLowerInvariant()
}

# Whether a file or folder exists: 'present', 'gone', or why that cannot be told. Stricter than
# Test-Path, which answers $false when a path cannot be read at all, so "cannot tell" is never
# taken for "gone".
function Get-PathState([string]$Path) {
    try {
        [void][IO.File]::GetAttributes($Path)
        return 'present'
    }
    catch [IO.FileNotFoundException], [IO.DirectoryNotFoundException] {
        return 'gone'
    }
    catch {
        $e = $_.Exception
        if ($e.InnerException) { $e = $e.InnerException }
        return "cannot tell ($($e.Message))"
    }
}

# The rollout files of a thread under a Codex home: Paths (full paths) and Error. A home without a
# sessions folder has none; any error while looking is returned as Error, never as "none".
function Get-Rollouts([string]$CodexHome, [string]$ThreadId) {
    try {
        $sessions = Join-Path $CodexHome 'sessions'
        $state = Get-PathState $sessions
        if ($state -eq 'gone') { return [pscustomobject]@{ Paths = @(); Error = $null } }
        if ($state -ne 'present') { return [pscustomobject]@{ Paths = @(); Error = "$sessions $state" } }
        $found = @(Get-ChildItem -LiteralPath $sessions -Recurse -File -Filter "*$ThreadId*" -ErrorAction Stop |
            ForEach-Object { $_.FullName })
        return [pscustomobject]@{ Paths = $found; Error = $null }
    }
    catch {
        return [pscustomobject]@{ Paths = @(); Error = $_.Exception.Message }
    }
}

# (e), before the sweep: the thread's rollout must be there to be removed. Returns the exact paths
# found, which Test-RolloutGone checks after the sweep. Not finding one fails, so the check after
# the sweep can never pass without having seen a rollout.
function Find-Rollout([string]$Name, [string]$CodexHome, [string]$ThreadId) {
    $found = Get-Rollouts $CodexHome $ThreadId
    if ($found.Error) {
        Test-Check "(e) $Name's thread has a rollout before the sweep" $false "listing its rollouts failed: $($found.Error)"
        return @()
    }
    Test-Check "(e) $Name's thread has a rollout before the sweep" ($found.Paths.Count -gt 0) $(
        if ($found.Paths.Count -gt 0) { $found.Paths -join ', ' } else { "none found for $ThreadId under $CodexHome\sessions" })
    return @($found.Paths)
}

# (e), after the sweep: every rollout Find-Rollout saw is gone, by its exact path, and no other
# rollout of the thread is left. Fails when none was seen before, and when either cannot be told.
function Test-RolloutGone([string]$Name, [string]$CodexHome, [string]$ThreadId, [string[]]$Before) {
    $label = "(e) the rollout of $Name's thread is gone"
    $seen = @($Before | Where-Object { $_ })
    if ($seen.Count -eq 0) {
        Test-Check $label $false 'no rollout was found before the sweep, so its removal cannot be shown'
        return
    }
    $wrong = @()
    foreach ($path in $seen) {
        $state = Get-PathState $path
        if ($state -ne 'gone') { $wrong += "$path is $state" }
    }
    $after = Get-Rollouts $CodexHome $ThreadId
    if ($after.Error) { $wrong += "listing its rollouts failed: $($after.Error)" }
    foreach ($path in @($after.Paths | Where-Object { $seen -notcontains $_ })) { $wrong += "$path is present" }
    Test-Check $label ($wrong.Count -eq 0) $(if ($wrong.Count -eq 0) { $seen -join ', ' } else { $wrong -join '; ' })
}

# Check one generate or refine result against the design (docs/design.md, "Success result").
function Test-ImageResult([string]$Label, $Result, [string]$Session, [int]$Version, [string]$Sent, [string]$SentWhat) {
    $text = Get-ResultText $Result
    $head = ($text -split "`n" | Select-Object -First 3) -join ' | '
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
    $first = ($text -split "`n" | Select-Object -First 1)
    Test-Check "$Label is version $Version of $Session" ($first -ceq "session: $Session   version: $Version") $first
    $expected = Join-Path $outDir "$Session-v$Version.png"
    $named = $text -match ('(?m)^image: ' + [regex]::Escape($expected) + '  \(')
    Test-Check "$Label names $Session-v$Version.png in the output folder" $named
    Test-Check "$Label file exists" (Test-Path -LiteralPath $expected) $expected
    # V2, compared here rather than taken from the server's own comparison: the prompt Codex says
    # it used, JSON-quoted on its line, must equal the text sent, whitespace at either end aside.
    $promptLines = @($text -split "`n" | Where-Object { $_ -match '^codex prompt: ' })
    if ($promptLines.Count -ne 1 -or $promptLines[0] -eq 'codex prompt: (not reported)') {
        Test-Check "$Label Codex reported the prompt it used (V2)" $false ($promptLines -join ' | ')
    }
    else {
        try {
            $reported = [string](ConvertFrom-Json -InputObject $promptLines[0].Substring('codex prompt: '.Length))
            Test-Check "$Label Codex used the $SentWhat verbatim (V2)" ($reported.Trim() -ceq $Sent.Trim()) $reported
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

$servers = New-Object System.Collections.Generic.List[object]
# The sessions made here, each with the number of images it should have.
$sessionsMade = [ordered]@{}
# Set once V4's refine has been cancelled; its stderr checks run after the servers stop.
$script:v4Cancelled = $false
$session = "smoke-$stamp"
$v1 = $null
$codexPid = $null
try {
    $s1 = Start-Server 'server 1'
    $servers.Add($s1)
    Write-Host '-> initialize'
    $init = Initialize-Server $s1
    Test-Check 'initialize negotiates 2025-11-25' ($init.protocolVersion -eq '2025-11-25') $init.protocolVersion

    Write-Host '-> tools/list'
    $tools = Invoke-Mcp $s1 'tools/list' @{} 30
    $names = @($tools.tools | ForEach-Object { $_.name })
    $wanted = @('codex_imagegen_generate', 'codex_imagegen_refine', 'codex_imagegen_status')
    Test-Check 'tools/list offers the three tools' (-not (Compare-Object $names $wanted)) ($names -join ', ')

    Write-Host '-> status (starts Codex; free calls only)'
    $status = Get-ResultText (Invoke-Mcp $s1 'tools/call' @{ name = 'codex_imagegen_status'; arguments = @{} } 120)
    Write-Host ($status -replace '(?m)^', '      ') -ForegroundColor DarkGray
    Test-Check 'status: image generation is available' ($status -match '(?m)^image generation: available') ''
    Test-Check 'status: the isolated state base has no sessions' ($status -match '(?m)^sessions in this project: none') ''
    Test-Check 'status: the state folder is under the run''s own state base' ($status.Contains("(state base $stateBase)")) ''
    if ($CodexHome) {
        $reported = if ($status -match '(?m)^Codex home: (.+) \(dedicated, from --codex-home\)\r?$') { $Matches[1] } else { $null }
        Test-Check 'V5: status reports the dedicated Codex home' ($null -ne $reported -and (Test-SameFolder $reported $CodexHome)) $(
            if ($status -match '(?m)^Codex home: .*$') { $Matches[0] } else { 'no Codex home line' })
    }
    if ($status -match 'app-server: running \(pid (\d+)\)') {
        $codexPid = [int]$Matches[1]
        Watch-Descendants $s1
        Test-Check 'the Codex app-server runs under the server' (@($s1.Descendants | Where-Object { $_.ProcessId -eq $codexPid }).Count -eq 1) "pid $codexPid"
    }

    if ($SpendQuota) {
        $e = [char]0x00E9
        $u = [char]0x00FC
        $dash = [char]0x2014
        $prompt = 'A small "brass" lighthouse on a rocky islet at dusk, loose watercolour.' + "`n" +
            'A signboard by its door reads C:\keeper ' + $dash + ' Caf' + $e + ' Br' + $u + 'cke.'
        Write-Host "-> (a) generate $session (1 image)"
        $result = Invoke-Tool $s1 'codex_imagegen_generate' @{ prompt = $prompt; session = $session; output_dir = $outDir }
        $v1 = Test-ImageResult 'generate' $result $session 1 $prompt 'prompt'
        if ($result.isError -ne $true) { $sessionsMade[$session] = 1 }

        if ($Interrupt) {
            if (-not $sessionsMade.Contains($session)) { throw '(a) made no image, so there is no session to refine for V4' }
            Write-Host "-> (i) V4: refine $session, cancelled once its image call is under way (1 image cut short)"
            $v4Id = Send-Mcp $s1 'tools/call' @{ name = 'codex_imagegen_refine'; arguments = @{ session = $session; feedback = 'Add a small red fishing boat by the rocks.' } } -WithProgress
            $answer = (Wait-ImageStarted $s1 $v4Id 300).Response
            if ($null -eq $answer) {
                # Well into the image call, as a user pressing Esc would be.
                $early = Read-ResponsesTo $s1 $v4Id 5
                if ($early.Count) { $answer = $early[0] }
            }
            if ($null -ne $answer) {
                Test-Check 'V4: the refine was still generating when it was cancelled' $false 'it answered before it could be cancelled'
                if ($answer.result -and $answer.result.isError -ne $true) { $sessionsMade[$session] = 2 }
            }
            else {
                Write-Host "-> notifications/cancelled for request $v4Id"
                $s1.Stdin.Write((ConvertTo-Json -InputObject ([ordered]@{ jsonrpc = '2.0'; method = 'notifications/cancelled'; params = [ordered]@{ requestId = $v4Id; reason = 'smoke V4' } }) -Compress) + "`n")
                $s1.Stdin.Flush()
                $cancelledAt = Get-Date
                $script:v4Cancelled = $true
                $late = Read-ResponsesTo $s1 $v4Id 30
                Test-Check 'V4: the cancelled request gets no response' ($late.Count -eq 0) $(if ($late.Count) { ((Get-ResultText $late[0].result) -split "`n" | Select-Object -First 2) -join ' | ' })
                if ($late.Count -and $late[0].result -and $late[0].result.isError -ne $true) { $sessionsMade[$session] = 2 }
                $running = ''
                for ($i = 0; $i -lt 10; $i++) {
                    $running = @((Get-Status $s1) -split "`n" | Where-Object { $_ -like 'running turns:*' }) -join ''
                    if ($running -eq 'running turns: none') { break }
                    Start-Sleep -Seconds 3
                }
                Test-Check 'V4: the interrupted turn is no longer running' ($running -eq 'running turns: none') $running
                # Codex could still finish an image it had in flight: wait as long as one takes.
                $pause = 60 - ((Get-Date) - $cancelledAt).TotalSeconds
                if ($pause -gt 0) {
                    Write-Host ("      waiting {0:N0} s for an image that might still arrive" -f $pause) -ForegroundColor DarkGray
                    Start-Sleep -Seconds ([int][Math]::Ceiling($pause))
                }
                $published = @(Get-ChildItem -LiteralPath $outDir -File -Filter "$session-v*.png" -ErrorAction SilentlyContinue | ForEach-Object { $_.Name })
                Test-Check 'V4: no file was published for the cancelled refine' ($published.Count -eq 1 -and $published[0] -eq "$session-v1.png") ($published -join ', ')
                $v4Record = Get-Record (Read-Store) $session
                if ($null -eq $v4Record) {
                    Test-Check "V4: $session is still recorded" $false ''
                }
                else {
                    $v4Outputs = Get-Field $v4Record 'outputs'
                    $turns = [int](Get-Field $v4Record 'turns')
                    Test-Check 'V4: the session record still holds its one image' ($turns -eq 1 -and @($v4Outputs | Where-Object { $_ }).Count -eq 1) "turns $turns, $(@($v4Outputs | Where-Object { $_ }).Count) output(s)"
                    $v4Folder = Join-Path (Join-Path ([string](Get-Field $v4Record 'codex_home')) 'generated_images') ([string](Get-Field $v4Record 'thread_id'))
                    $codexPngs = @(Get-ChildItem -LiteralPath $v4Folder -File -Filter '*.png' -ErrorAction SilentlyContinue | ForEach-Object { $_.Name })
                    Test-Check 'V4: Codex saved no image for the cancelled refine' ($codexPngs.Count -eq 1) "$($codexPngs.Count) PNG(s) in $v4Folder"
                }
            }
            Stop-Server $s1
        }
        else {
            $feedback = 'Make the sky a deep "violet" and add one gull.' + "`n" +
                'Repaint the sign to read D:\harbour ' + $dash + ' ' + $u + 'ber ' + $e + 'toile.'
            Write-Host "-> (b) refine $session (1 image)"
            $result = Invoke-Tool $s1 'codex_imagegen_refine' @{ session = $session; feedback = $feedback }
            [void](Test-ImageResult 'refine' $result $session 2 $feedback 'feedback')
            if ($result.isError -ne $true) { $sessionsMade[$session] = 2 }

            Write-Host '-> (c) kill the server, start a fresh one on the same state, refine again (1 image)'
            Stop-Server $s1 -Kill
            $s2 = Start-Server 'server 2'
            $servers.Add($s2)
            [void](Initialize-Server $s2)
            $feedback2 = 'Now make it night, with the lighthouse lamp lit.'
            $result = Invoke-Tool $s2 'codex_imagegen_refine' @{ session = $session; feedback = $feedback2 }
            [void](Test-ImageResult 'refine after a restart' $result $session 3 $feedback2 'feedback')
            if ($result.isError -ne $true) { $sessionsMade[$session] = 3 }

            Write-Host '-> (d) status'
            $status2 = Get-ResultText (Invoke-Mcp $s2 'tools/call' @{ name = 'codex_imagegen_status'; arguments = @{} } 120)
            Write-Host ($status2 -replace '(?m)^', '      ') -ForegroundColor DarkGray
            Test-Check 'status lists the session with 3 turns' ($status2 -match ('(?m)^  ' + [regex]::Escape($session) + ': 3 turns, latest ' + [regex]::Escape((Join-Path $outDir "$session-v3.png")))) ''
            Test-Check 'status: no turn is left running' ($status2 -match '(?m)^running turns: none') ''
            Stop-Server $s2
        }

        if ($Concurrent) {
            Write-Host '-> V9: two servers generate at the same moment (2 images)'
            $a = Start-Server 'V9 server A'
            $b = Start-Server 'V9 server B'
            $servers.Add($a)
            $servers.Add($b)
            [void](Initialize-Server $a)
            [void](Initialize-Server $b)
            $pair = @(
                [pscustomobject]@{ Server = $a; Session = "$session-a"; Prompt = 'A red kite over green hills, flat vector style.' },
                [pscustomobject]@{ Server = $b; Session = "$session-b"; Prompt = 'A blue kite over a grey sea, flat vector style.' }
            )
            foreach ($p in $pair) {
                $p | Add-Member -NotePropertyName Id -NotePropertyValue (Send-Mcp $p.Server 'tools/call' @{ name = 'codex_imagegen_generate'; arguments = @{ prompt = $p.Prompt; session = $p.Session; output_dir = $outDir } } -WithProgress)
            }
            foreach ($p in $pair) {
                try {
                    $result = Receive-Mcp $p.Server $p.Id 'tools/call' 420
                    Write-Host ((Get-ResultText $result) -replace '(?m)^', '      ') -ForegroundColor DarkGray
                    [void](Test-ImageResult $p.Server.Label $result $p.Session 1 $p.Prompt 'prompt')
                    if ($result.isError -ne $true) { $sessionsMade[$p.Session] = 1 }
                }
                catch {
                    Test-Check "$($p.Server.Label) answers" $false $_.Exception.Message
                }
            }
            Stop-Server $a
            Stop-Server $b
            $store = Read-Store
            foreach ($p in $pair) {
                Test-Check "V9: $($p.Session) is recorded in the shared store" ($null -ne (Get-Record $store $p.Session)) ''
            }
        }
    }
    else {
        Stop-Server $s1
    }
}
catch {
    Test-Check 'the MCP conversation' $false $_.Exception.Message
}

# Whatever the run got to, no server of ours is left running.
foreach ($s in $servers) {
    if (-not $s.Proc.HasExited) { Stop-Server $s }
}

# ---------------------------------------------------------------------------------------------
# V1 and V3, from Codex's rollout trace and the servers' stderr. Before the cleanup, which removes
# the files they compare.
# ---------------------------------------------------------------------------------------------

$store = $null
if ($SpendQuota) {
    $store = Read-Store
    $record = if ($store) { Get-Record $store $session } else { $null }
    # Not $codexHome: PowerShell names ignore case, and that would overwrite -CodexHome.
    $recordHome = if ($record) { [string](Get-Field $record 'codex_home') } elseif ($CodexHome) { $CodexHome } else { $ambientHome }
    $imagesRoot = [IO.Path]::GetFullPath((Join-Path $recordHome 'generated_images')).TrimEnd('\') + '\'
    $allStderr = (@($servers | ForEach-Object { $_.Stderr } | Where-Object { $_ }) -join "`n")

    # The first image's savedPath is the refine's first choice of edit target; our copy of it is
    # the fallback. Either in referenced_image_paths passes V3 for refine.
    $savedLines = @([regex]::Matches($allStderr, '(?m)^codex-imagegen: session ([A-Za-z0-9._-]+): image item has savedPath (.+?)\r?$'))
    $firstSaved = @($savedLines | Where-Object { $_.Groups[1].Value -eq $session } | Select-Object -First 1 | ForEach-Object { $_.Groups[2].Value })
    Invoke-TraceChecks $traceRoot (@($firstSaved) + @($v1) | Where-Object { $_ }) (Join-Path $work 'v1-tools.txt') 'V3 (refine): the edit target reached the image tool''s referenced_image_paths'

    # V1's last part: Codex reports savedPath. The result reads the same when the server falls
    # back to the image's base64, so the server logs where each image came from (turn.rs,
    # image_source_line), and each copy it published must match Codex's file byte for byte. Each
    # turn makes one image, so a session's k-th savedPath line is its version k.
    if (-not $allStderr) {
        Add-Check 'V1: Codex reported savedPath for each image' 'MANUAL' 'the servers'' stderr was not captured'
    }
    else {
        $fallbacks = @([regex]::Matches($allStderr, '(?m)^codex-imagegen: session [A-Za-z0-9._-]+: image item has no savedPath.*$'))
        Test-Check 'V1: no image lacked savedPath (no base64 fallback)' ($fallbacks.Count -eq 0) (@($fallbacks | ForEach-Object { $_.Value.Trim() }) -join ' | ')
        foreach ($name in $sessionsMade.Keys) {
            $mine = @($savedLines | Where-Object { $_.Groups[1].Value -eq $name })
            if ($mine.Count -ne $sessionsMade[$name]) {
                Test-Check "V1: Codex reported savedPath for each image of $name" $false "$($mine.Count) savedPath line(s) for $($sessionsMade[$name]) image(s)"
                continue
            }
            for ($k = 0; $k -lt $mine.Count; $k++) {
                $savedPath = $mine[$k].Groups[2].Value
                $copy = Join-Path $outDir "$name-v$($k + 1).png"
                $same = $savedPath.StartsWith($imagesRoot, [StringComparison]::OrdinalIgnoreCase) -and
                    (Test-Path -LiteralPath $savedPath) -and (Test-Path -LiteralPath $copy) -and
                    ((Get-FileHash -LiteralPath $savedPath).Hash -eq (Get-FileHash -LiteralPath $copy).Hash)
                Test-Check "V1: Codex reported savedPath for $name v$($k + 1), and $name-v$($k + 1).png is a byte copy of it" $same $savedPath
            }
        }
    }

    # V4, from the servers' stderr (turn.rs, send_interrupt and route_notification). The session's
    # thread ran two turns, the generate's and then the refine's, and it is the refine's turn, the
    # second to complete, that must have been interrupted: named by its completion, not by the first
    # interrupt seen, which could be the generate's.
    if ($script:v4Cancelled) {
        $threadId = if ($record) { [string](Get-Field $record 'thread_id') } else { '' }
        $completions = @(if ($threadId) { [regex]::Matches($allStderr, '(?m)^codex-imagegen: turn (\S+) on thread ' + [regex]::Escape($threadId) + ' completed: (\S+)\r?$') })
        Test-Check 'V4: Codex completed the session''s two turns' ($completions.Count -eq 2) "$($completions.Count) turn/completed line(s) for thread $threadId"
        if ($completions.Count -eq 2) {
            $turnId = $completions[1].Groups[1].Value
            $turnStatus = $completions[1].Groups[2].Value
            $sent = $allStderr -match ('(?m)^codex-imagegen: interrupting turn ' + [regex]::Escape($turnId) + ' on thread ' + [regex]::Escape($threadId) + '\r?$')
            Test-Check 'V4: the server sent turn/interrupt for the refine''s turn' $sent "turn $turnId"
            Test-Check 'V4: Codex completed the refine''s turn as interrupted' ($turnStatus -eq 'interrupted') "turn $turnId completed: $turnStatus"
        }
        # The thread's only image item is the first image, completed: a cut-short call must send
        # no item/completed, failed or not. Logged as it arrives, even after its call gave up.
        $itemFailures = @(if ($threadId) { [regex]::Matches($allStderr, '(?m)^codex-imagegen: image item failed on thread ' + [regex]::Escape($threadId) + ': .*$') | ForEach-Object { $_.Value.Trim() } })
        Test-Check 'V4: Codex sent no failed image item for the cut-short call' ($threadId -and $itemFailures.Count -eq 0) ($itemFailures -join ' | ')
    }

    # V5: every session the run made is recorded against the dedicated home, every image Codex
    # saved is under it, and nothing of the run's threads is in the ambient home.
    if ($CodexHome) {
        $dedicatedImages = (Join-Path $CodexHome 'generated_images').TrimEnd('\') + '\'
        $outside = @($savedLines | Where-Object { -not $_.Groups[2].Value.StartsWith($dedicatedImages, [StringComparison]::OrdinalIgnoreCase) } | ForEach-Object { $_.Groups[2].Value })
        Test-Check 'V5: every image Codex saved is under the dedicated home' ($savedLines.Count -gt 0 -and $outside.Count -eq 0) $(
            if ($savedLines.Count -eq 0) { 'no savedPath was logged' } else { $outside -join ', ' })
        foreach ($name in $sessionsMade.Keys) {
            $made = if ($store) { Get-Record $store $name } else { $null }
            if ($null -eq $made) { continue }
            $recordedHome = [string](Get-Field $made 'codex_home')
            Test-Check "V5: $name's record names the dedicated home" (Test-SameFolder $recordedHome $CodexHome) $recordedHome
            $threadId = [string](Get-Field $made 'thread_id')
            $others = @($ambientHomes | Where-Object { -not (Test-SameFolder $_ $CodexHome) } | Sort-Object -Unique)
            if ($others.Count -eq 0) {
                Add-Check "V5: nothing of $name's thread is in the ambient home" 'MANUAL' 'every ambient home is the dedicated home, so there is nothing to compare with'
            }
            foreach ($other in $others) {
                $ambientRollouts = Get-Rollouts $other $threadId
                $ambientFolder = Join-Path (Join-Path $other 'generated_images') $threadId
                $ambientState = Get-PathState $ambientFolder
                $clean = (-not $ambientRollouts.Error) -and $ambientRollouts.Paths.Count -eq 0 -and $ambientState -eq 'gone'
                Test-Check "V5: nothing of $name's thread is in the ambient home $other" $clean $(
                    if ($ambientRollouts.Error) { "listing failed: $($ambientRollouts.Error)" } else { @(@($ambientRollouts.Paths) + @("$ambientFolder is $ambientState")) -join ', ' })
            }
        }
    }
    Write-Host "      the trace holds prompts and tool I/O; delete $traceRoot when done" -ForegroundColor DarkGray
}

# ---------------------------------------------------------------------------------------------
# (e) The manual sweep, free: thread/delete and file deletions only
# ---------------------------------------------------------------------------------------------

# On a paid run it runs whatever the run got to, and covers every session the isolated store
# records, not only the ones the run kept track of: a result lost after its image completed (a
# closed stream, a parse that threw) still left a record, and a thread in the real Codex home that
# nothing else would ever remove. The store is the run's own, so the sweep can touch nothing else.
# A tracked session's files are named independently of its record: the output folder is fresh, so
# <session>-v1..vN are exactly the files the run published.

if ($SpendQuota) {
    $recorded = @()
    $sessionMap = Get-Field $store 'sessions'
    if ($sessionMap -is [Collections.IDictionary]) {
        $recorded = @($sessionMap.Values)
    }
    elseif (@(Get-ChildItem -LiteralPath $stateBase -Recurse -File -Filter 'sessions.json' -ErrorAction SilentlyContinue).Count -gt 0) {
        Test-Check '(e) the run''s session store can be read' $false 'the sweep may leave the run''s sessions behind'
    }
    # What each session left, read before the sweep removes it.
    $left = @()
    foreach ($record in $recorded) {
        $name = [string](Get-Field $record 'name')
        $threadId = [string](Get-Field $record 'thread_id')
        $sessionHome = [string](Get-Field $record 'codex_home')
        # Assigned first, not piped: Get-Field hands an array over as one pipeline object.
        $published = Get-Field $record 'outputs'
        $outputs = @()
        foreach ($o in @($published | Where-Object { $_ })) { $outputs += [string](Get-Field $o 'path') }
        $expected = @()
        if ($sessionsMade.Contains($name)) {
            $expected = @(1..$sessionsMade[$name] | ForEach-Object { Join-Path $outDir "$name-v$_.png" })
            $sameList = $outputs.Count -eq $expected.Count -and
                -not (Compare-Object @($outputs | ForEach-Object { $_.ToLowerInvariant() }) @($expected | ForEach-Object { $_.ToLowerInvariant() }))
            Test-Check "(e) $name's record lists exactly the $($expected.Count) file(s) the run published" $sameList ($outputs -join ', ')
            $missing = @($expected | Where-Object { -not (Test-Path -LiteralPath $_) })
            Test-Check "(e) $name's $($expected.Count) published file(s) exist before the sweep" ($missing.Count -eq 0) ($missing -join ', ')
        }
        else {
            Test-Check "(e) $name was tracked by the run" $false 'recorded in the store, but no result the run read reported it; the sweep still removes it'
        }
        $rollouts = @(Find-Rollout $name $sessionHome $threadId)
        $left += [pscustomobject]@{ Name = $name; ThreadId = $threadId; Home = $sessionHome; Outputs = $outputs; Expected = $expected; Rollouts = $rollouts }
    }
    foreach ($name in $sessionsMade.Keys) {
        if (-not @($left | Where-Object { $_.Name -eq $name })) { Test-Check "$name is recorded in the store" $false '' }
    }
    Write-Host '-> (e) codex-imagegen.exe --cleanup --older-than-days 0 (free)'
    $sweep = Invoke-Cleanup
    Test-Check '(e) --cleanup exits 0' ($sweep.Exited -and $sweep.ExitCode -eq 0) "exit code $($sweep.ExitCode)"
    $after = Read-Store
    foreach ($s in $left) {
        Test-Check "(e) --cleanup reports $($s.Name) removed" ($sweep.Out -match ('(?m)^removed [^:]+: ' + [regex]::Escape($s.Name) + ' \(')) ''
        Test-Check "(e) $($s.Name)'s record is gone" ($null -eq (Get-Record $after $s.Name)) ''
        # Every file the run published for it and every one its record listed, plus anything else
        # of its name in the output folder.
        $files = @(@($s.Expected) + @($s.Outputs) | Where-Object { $_ } | Sort-Object -Unique)
        $strays = @(Get-ChildItem -LiteralPath $outDir -File -Filter "$($s.Name)-v*.png" -ErrorAction SilentlyContinue | ForEach-Object { $_.FullName })
        $remaining = @(@($files) + @($strays) | Where-Object { $_ -and (Test-Path -LiteralPath $_) } | Sort-Object -Unique)
        Test-Check "(e) $($s.Name)'s $($files.Count) published file(s) are gone" ($files.Count -gt 0 -and $remaining.Count -eq 0) ($remaining -join ', ')
        $imageDir = Join-Path (Join-Path $s.Home 'generated_images') $s.ThreadId
        Test-Check "(e) Codex's image folder for $($s.Name) is gone" (-not (Test-Path -LiteralPath $imageDir)) $imageDir
        Test-RolloutGone $s.Name $s.Home $s.ThreadId $s.Rollouts
    }
    if ($left.Count -eq 0) {
        Add-Check '(e) --cleanup removes the run''s sessions' 'SKIP' 'the store holds no session'
    }
}
else {
    Write-Host '-> --cleanup --older-than-days 0 on the empty state base (free)'
    $sweep = Invoke-Cleanup
    Test-Check '--cleanup exits 0' ($sweep.Exited -and $sweep.ExitCode -eq 0) "exit code $($sweep.ExitCode)"
    Test-Check '--cleanup finds nothing to remove in a fresh state base' ($sweep.Out -match '(?m)^nothing to remove') ''
}

# ---------------------------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------------------------

if ($SpendQuota) {
    Complete-Smoke 'SMOKE PASS' "Working folder: $work"
}
Complete-Smoke 'SMOKE PASS (free steps only)'
