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

  Run it after .\build.ps1, when a change touches the protocol, spawning or turn handling. Tell
  the owner the cost before running it with -SpendQuota.
#>
[CmdletBinding()]
param(
    # Required to generate images. Without it only the free steps run.
    [switch]$SpendQuota,
    # The server to test. Default: dist\codex-imagegen.exe next to this script.
    [string]$Exe,
    # Passed to the server as --codex-bin when given.
    [string]$CodexBin
)

# Not 'Stop': failures are collected as checks and summarised at the end.
$ErrorActionPreference = 'Continue'
Set-StrictMode -Version 2.0

# Resolved here rather than as the parameter's default: Windows PowerShell 5.1 leaves
# $PSScriptRoot empty while it binds parameters.
if (-not $Exe) { $Exe = Join-Path $PSScriptRoot 'dist\codex-imagegen.exe' }

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
$utf8 = New-Object System.Text.UTF8Encoding($false)
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

# Whether $Object has a property called $Name (Set-StrictMode makes reading a missing one an error).
function Test-Property($Object, [string]$Name) {
    return ($null -ne $Object) -and ($Object.PSObject.Properties.Name -contains $Name)
}

if ($SpendQuota) {
    Write-Host '-> V1: tools offered to the agent model (rollout trace)'
    # Each thread writes <root>\trace-<uuid>-<thread>\trace.jsonl, one event per line, each naming
    # its payload files by a path relative to that folder (codex-rs rollout-trace at rust-v0.156.0,
    # raw_event.rs and payload.rs):
    # - inference_started names the Responses request, whose `tools` list is what the model saw.
    #   Nested tools reachable from code-mode `exec` are listed in its description as "### `name`"
    #   headings (code-mode-protocol description.rs).
    # - tool_call_started names the invocation of each tool Codex ran, nested calls from `exec`
    #   included: {tool_name, tool_namespace, payload: {type: "function", arguments: "<JSON>"}}
    #   (tool_dispatch.rs). The image tool's is how V3 is checked.
    $topLevel = New-Object System.Collections.Generic.HashSet[string]
    $nested = New-Object System.Collections.Generic.HashSet[string]
    $requests = 0
    $imageCalls = @()
    $traceError = $null
    try {
        foreach ($log in @(Get-ChildItem -LiteralPath $traceRoot -Recurse -Filter 'trace.jsonl' -ErrorAction SilentlyContinue)) {
            foreach ($line in [IO.File]::ReadAllLines($log.FullName, $utf8)) {
                if ($line -notmatch '"(inference_started|tool_call_started)"') { continue }
                $traceEvent = ConvertFrom-Json -InputObject $line
                if ($traceEvent.payload.type -eq 'inference_started') {
                    $payloadPath = Join-Path $log.DirectoryName $traceEvent.payload.request_payload.path
                    if (-not (Test-Path -LiteralPath $payloadPath)) { continue }
                    $request = ConvertFrom-Json -InputObject ([IO.File]::ReadAllText($payloadPath, $utf8))
                    $requests++
                    foreach ($tool in @($request.tools)) {
                        $toolName = if (Test-Property $tool 'name') { $tool.name } else { $tool.type }
                        [void]$topLevel.Add($toolName)
                        if (Test-Property $tool 'tools') {
                            foreach ($inner in @($tool.tools)) { [void]$nested.Add("$toolName.$($inner.name)") }
                        }
                        if ($toolName -eq 'exec' -and (Test-Property $tool 'description')) {
                            foreach ($m in [regex]::Matches($tool.description, '(?m)^### `([^`]+)`')) {
                                [void]$nested.Add($m.Groups[1].Value)
                            }
                        }
                    }
                }
                elseif ($traceEvent.payload.type -eq 'tool_call_started') {
                    $ref = if (Test-Property $traceEvent.payload 'invocation_payload') { $traceEvent.payload.invocation_payload } else { $null }
                    if ($null -eq $ref) { continue }
                    $invocationPath = Join-Path $log.DirectoryName $ref.path
                    if (-not (Test-Path -LiteralPath $invocationPath)) { continue }
                    $invocation = ConvertFrom-Json -InputObject ([IO.File]::ReadAllText($invocationPath, $utf8))
                    # Matched on the tool's name alone, so a change of namespace does not hide it.
                    if ($invocation.tool_name -ne 'imagegen' -or -not (Test-Property $invocation.payload 'arguments')) { continue }
                    $arguments = ConvertFrom-Json -InputObject $invocation.payload.arguments
                    $paths = @()
                    if (Test-Property $arguments 'referenced_image_paths') {
                        $paths = @($arguments.referenced_image_paths | Where-Object { $_ } | ForEach-Object { [IO.Path]::GetFullPath([string]$_) })
                    }
                    $imageCalls += [pscustomobject]@{ At = [long]$traceEvent.wall_time_unix_ms; Paths = $paths }
                }
            }
        }
    }
    catch {
        $traceError = $_.Exception.Message
    }
    $toolReport = Join-Path $work 'v1-tools.txt'
    $reportLines = @("requests inspected: $requests", "top-level tools: $(@($topLevel) -join ', ')", "nested tools: $(@($nested) -join ', ')",
        "image tool calls: $($imageCalls.Count)")
    foreach ($call in $imageCalls) { $reportLines += "  referenced_image_paths: [$(@($call.Paths) -join ', ')]" }
    [IO.File]::WriteAllLines($toolReport, $reportLines, $utf8)
    foreach ($l in $reportLines) { Write-Host "      $l" -ForegroundColor DarkGray }
    if ($traceError) {
        Add-Check 'V1: the rollout trace is readable' 'MANUAL' "$traceError; inspect $traceRoot by hand"
    }
    if ($requests -eq 0) {
        Add-Check 'V1: tool list' 'MANUAL' "no inference request found in the trace under $traceRoot; inspect it by hand"
    }
    else {
        Test-Check 'V1: exec is offered' ($topLevel.Contains('exec')) ''
        Test-Check 'V1: the image tool is nested in exec' (@($nested | Where-Object { $_ -match 'imagegen' }).Count -gt 0) ''
        $forbidden = 'shell|exec_command|write_stdin|web_search|browser|computer|spawn_agent|send_input|wait_agent|close_agent|resume_agent|followup_task|send_message|list_agents|interrupt_agent|skill|tool_suggest|request_plugin_install|list_available_plugins'
        $offending = @(@($topLevel) + @($nested) | Where-Object { $_ -match $forbidden })
        Test-Check 'V1: no shell, stdin, web search, browser, computer-use, multi-agent, skill or tool-suggest tool' ($offending.Count -eq 0) ($offending -join ', ')
        Write-Host "      full list saved to $toolReport" -ForegroundColor DarkGray
    }

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

    # V3's first half: the reference reached the image tool's referenced_image_paths, as Codex
    # recorded the call. Never judged from text Codex sent to the model: the developer instructions,
    # the tool's declaration and the tagged input already name both the parameter and the path.
    if ($v1) {
        $v3Name = 'V3: the reference reached the image tool''s referenced_image_paths'
        $v1Full = [IO.Path]::GetFullPath($v1)
        # Only the second thread's call can name the first image's copy: it did not exist before.
        if (@($imageCalls | Where-Object { $_.Paths -contains $v1Full }).Count -gt 0) {
            Add-Check $v3Name 'PASS' $v1Full
        }
        elseif ($imageCalls.Count -ge 2) {
            $last = @($imageCalls | Sort-Object At)[-1]
            Test-Check $v3Name $false "the last image call had referenced_image_paths: [$(@($last.Paths) -join ', ')]"
        }
        else {
            Add-Check $v3Name 'MANUAL' "$($imageCalls.Count) image tool call(s) found in the trace; inspect $traceRoot by hand"
        }
    }
    Write-Host "      the trace holds prompts and tool I/O; delete $traceRoot when done" -ForegroundColor DarkGray
}

# ---------------------------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------------------------

$failed = @($script:checks | Where-Object { $_.Outcome -eq 'FAIL' })
$manual = @($script:checks | Where-Object { $_.Outcome -eq 'MANUAL' })
Write-Host ''
Write-Host ("{0} checks: {1} passed, {2} failed, {3} to inspect by hand, {4} skipped" -f $script:checks.Count,
    @($script:checks | Where-Object { $_.Outcome -eq 'PASS' }).Count, $failed.Count, $manual.Count,
    @($script:checks | Where-Object { $_.Outcome -eq 'SKIP' }).Count)
if ($failed.Count -gt 0) {
    Write-Host 'SMOKE FAIL' -ForegroundColor Red
    foreach ($f in $failed) { Write-Host "  $($f.Name): $($f.Detail)" -ForegroundColor Red }
    exit 1
}
Write-Host $(if ($SpendQuota) { 'SMOKE PASS' } else { 'SMOKE PASS (free steps only)' }) -ForegroundColor Green
if ($SpendQuota) { Write-Host "Images: $outDir" }
exit 0
