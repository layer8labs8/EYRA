<#
.SYNOPSIS
  Real-task benchmark for EYRA: runs scripted desktop tasks through the MCP server and records
  success rate, tool calls, tokens and time, next to what the same observations would cost as
  screenshots.

.DESCRIPTION
  The steps are scripted (no LLM in the loop), so this measures whether EYRA's observations and
  actions are reliable and how much they cost. An agent's own success also depends on its model.

  Tasks (all local, nothing is saved or sent anywhere):
    1. notepad-type   Type a sentence into Notepad, confirm it with see_diff
    2. notepad-menu   Open Notepad's Edit menu by #id, read the menu items with see_diff
    3. form-submit    Fill a text box, tick a checkbox, press a button in a WinForms app
    4. explorer-find  Open a folder in File Explorer and find three files in the map
    5. wait-change    Wait for a label to change on its own and read the change

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File bench\run-tasks.ps1 -Runs 5
#>
param(
  [int]$Runs = 5,
  [string]$Eyra = (Join-Path $PSScriptRoot "..\target\release\eyra.exe"),
  [string]$Out = (Join-Path $PSScriptRoot "RESULTS.md"),
  [string[]]$Only = @()
)
$ErrorActionPreference = "Stop"
$Eyra = (Resolve-Path $Eyra).Path

# ------------------------------------------------------------------ MCP client
class Mcp {
  [System.Diagnostics.Process]$P
  [int]$Id = 0
  [int]$Calls = 0
  [long]$Chars = 0
  [int]$Observations = 0
  [long]$ShotTokens = 0

  Mcp([string]$exe) {
    $psi = New-Object System.Diagnostics.ProcessStartInfo $exe, "--no-log --confirm off mcp"
    $psi.RedirectStandardInput = $true
    $psi.RedirectStandardOutput = $true
    $psi.UseShellExecute = $false
    $psi.StandardOutputEncoding = [System.Text.Encoding]::UTF8
    $this.P = [System.Diagnostics.Process]::Start($psi)
    $this.Send("initialize", @{ protocolVersion = "2025-06-18" }) | Out-Null
  }

  [object] Send([string]$method, [hashtable]$params) {
    $this.Id++
    $req = @{ jsonrpc = "2.0"; id = $this.Id; method = $method; params = $params } | ConvertTo-Json -Compress -Depth 6
    $this.P.StandardInput.WriteLine($req)
    $this.P.StandardInput.Flush()
    return ($this.P.StandardOutput.ReadLine() | ConvertFrom-Json)
  }

  # Calls a tool and returns its text; counts tokens and, for observations, the screenshot cost
  [string] Tool([string]$name, [hashtable]$arguments) {
    $r = $this.Send("tools/call", @{ name = $name; arguments = $arguments })
    $text = [string]$r.result.content[0].text
    $this.Calls++
    $this.Chars += $text.Length
    if ($name -in @("see", "see_diff")) {
      $this.Observations++
      $m = [regex]::Match($text, 'rect=-?\d+,-?\d+,(\d+),(\d+)')
      if ($m.Success) { $this.ShotTokens += [math]::Ceiling([int]$m.Groups[1].Value * [int]$m.Groups[2].Value / 750) }
    }
    return $text
  }

  [void] Close() { try { $this.P.StandardInput.Close(); $this.P.WaitForExit(3000) | Out-Null } catch {} }
}

function Find-Id([string]$text, [string]$pattern) {
  $m = [regex]::Match($text, $pattern)
  if (-not $m.Success) { throw "pattern not found: $pattern" }
  return [int]$m.Groups[1].Value
}

function Wait-MainWindow([System.Diagnostics.Process]$proc) {
  for ($i = 0; $i -lt 50; $i++) {
    $proc.Refresh()
    if ($proc.MainWindowHandle -ne 0) { return ("0x{0:X}" -f [int64]$proc.MainWindowHandle) }
    Start-Sleep -Milliseconds 100
  }
  throw "window did not appear"
}

function Start-TestForm([string]$title, [string]$extra) {
  $script = Join-Path $env:TEMP "eyra_bench_form.ps1"
  @"
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
`$f = New-Object System.Windows.Forms.Form
`$f.Text = "$title"
`$f.Size = New-Object System.Drawing.Size(600, 360)
`$f.StartPosition = "Manual"
`$f.Location = New-Object System.Drawing.Point(80, 80)
`$lbl = New-Object System.Windows.Forms.Label
`$lbl.Text = "Status: Ready"
`$lbl.Location = New-Object System.Drawing.Point(20, 20)
`$lbl.Size = New-Object System.Drawing.Size(520, 32)
`$f.Controls.Add(`$lbl)
`$tb = New-Object System.Windows.Forms.TextBox
`$tb.AccessibleName = "Query Input"
`$tb.Location = New-Object System.Drawing.Point(20, 68)
`$tb.Size = New-Object System.Drawing.Size(320, 30)
`$f.Controls.Add(`$tb)
`$chk = New-Object System.Windows.Forms.CheckBox
`$chk.Text = "Enable Sync"
`$chk.Location = New-Object System.Drawing.Point(20, 112)
`$chk.Size = New-Object System.Drawing.Size(220, 30)
`$f.Controls.Add(`$chk)
`$btn = New-Object System.Windows.Forms.Button
`$btn.Text = "Run Action"
`$btn.Location = New-Object System.Drawing.Point(20, 160)
`$btn.Size = New-Object System.Drawing.Size(160, 40)
`$btn.Add_Click({ `$lbl.Text = "Updated:" + `$tb.Text + ":" + `$chk.Checked })
`$f.Controls.Add(`$btn)
$extra
`$k = New-Object System.Windows.Forms.Timer
`$k.Interval = 30000
`$k.Add_Tick({ `$f.Close() })
`$k.Start()
[System.Windows.Forms.Application]::Run(`$f)
"@ | Set-Content -Encoding ASCII $script
  return Start-Process powershell -ArgumentList "-NoProfile", "-STA", "-WindowStyle", "Hidden", "-File", $script -PassThru
}

# ------------------------------------------------------------------ tasks
# Each task gets a fresh MCP session and returns $true on success.

$tasks = [ordered]@{}

$tasks["notepad-type"] = {
  param($mcp)
  $np = Start-Process notepad -PassThru
  try {
    $hw = Wait-MainWindow $np
    $see = $mcp.Tool("see", @{ window = $hw; interactive_only = $true; coords = "center" })
    $edit = Find-Id $see '#(\d+) inp '
    $mcp.Tool("type_text", @{ id = $edit; text = "EYRA benchmark sentence 42" }) | Out-Null
    $diff = $mcp.Tool("see_diff", @{ window = $hw; interactive_only = $true; coords = "center" })
    return $diff.Contains("EYRA benchmark sentence 42")
  } finally { Stop-Process -Id $np.Id -Force -ErrorAction SilentlyContinue }
}

$tasks["notepad-menu"] = {
  param($mcp)
  $np = Start-Process notepad -PassThru
  try {
    $hw = Wait-MainWindow $np
    $see = $mcp.Tool("see", @{ window = $hw; interactive_only = $true; coords = "center" })
    # top-level menu items share the menu bar's row; the 2nd one is Edit in every language
    $items = [regex]::Matches($see, '#(\d+) mnu "([^"]+)"[^@]*@(\d+),(\d+)') |
      Where-Object { $_.Groups[2].Value -ne "System" } |
      ForEach-Object { [pscustomobject]@{ Id = [int]$_.Groups[1].Value; X = [int]$_.Groups[3].Value; Y = [int]$_.Groups[4].Value } }
    $row = ($items | Measure-Object -Property Y -Minimum).Minimum
    $bar = $items | Where-Object { [math]::Abs($_.Y - $row) -le 4 } | Sort-Object X
    $mcp.Tool("click", @{ id = $bar[1].Id }) | Out-Null
    Start-Sleep -Milliseconds 300
    $diff = $mcp.Tool("see_diff", @{ window = $hw; interactive_only = $true; coords = "center" })
    $mcp.Tool("hotkey", @{ keys = "esc"; window = $hw }) | Out-Null
    $mcp.Tool("hotkey", @{ keys = "esc"; window = $hw }) | Out-Null
    return ([regex]::Matches($diff, '\+ #\d+ mnu ').Count -ge 3)
  } finally { Stop-Process -Id $np.Id -Force -ErrorAction SilentlyContinue }
}

$tasks["form-submit"] = {
  param($mcp)
  $form = Start-TestForm "EYRA Bench Form" ""
  try {
    $hw = $null
    for ($i = 0; $i -lt 50 -and -not $hw; $i++) {
      Start-Sleep -Milliseconds 150
      $w = $mcp.Send("tools/call", @{ name = "list_windows"; arguments = @{} }).result.content[0].text
      $m = [regex]::Match($w, '(0x[0-9A-F]+) \| [^|]+\| "EYRA Bench Form"')
      if ($m.Success) { $hw = $m.Groups[1].Value }
    }
    if (-not $hw) { throw "form did not appear" }
    $see = $mcp.Tool("see", @{ window = $hw; interactive_only = $true; coords = "center" })
    $mcp.Tool("type_text", @{ id = (Find-Id $see '#(\d+) inp "Query Input"'); text = "bench" }) | Out-Null
    $mcp.Tool("click", @{ id = (Find-Id $see '#(\d+) chk "Enable Sync"') }) | Out-Null
    $mcp.Tool("click", @{ id = (Find-Id $see '#(\d+) btn "Run Action"') }) | Out-Null
    Start-Sleep -Milliseconds 200
    $diff = $mcp.Tool("see_diff", @{ window = $hw; coords = "center" })
    return $diff.Contains("Updated:bench:True")
  } finally { Get-CimInstance Win32_Process -Filter "ParentProcessId=$($form.Id)" -ErrorAction SilentlyContinue | ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }; Stop-Process -Id $form.Id -Force -ErrorAction SilentlyContinue }
}

$tasks["explorer-find"] = {
  param($mcp)
  $dir = Join-Path $env:TEMP ("eyra_bench_" + [guid]::NewGuid().ToString("N").Substring(0, 8))
  New-Item -ItemType Directory $dir | Out-Null
  foreach ($n in "bench_alpha.txt", "bench_bravo.txt", "bench_charlie.txt") { Set-Content (Join-Path $dir $n) "x" }
  $leaf = Split-Path $dir -Leaf
  Start-Process explorer.exe $dir
  try {
    $hw = $null
    for ($i = 0; $i -lt 60 -and -not $hw; $i++) {
      Start-Sleep -Milliseconds 150
      $w = $mcp.Send("tools/call", @{ name = "list_windows"; arguments = @{} }).result.content[0].text
      $m = [regex]::Match($w, "(0x[0-9A-F]+) \| explorer\.exe \| ""$leaf""")
      if ($m.Success) { $hw = $m.Groups[1].Value }
    }
    if (-not $hw) { throw "explorer window did not appear" }
    Start-Sleep -Milliseconds 400
    $see = $mcp.Tool("see", @{ window = $hw; interactive_only = $true; coords = "center" })
    return ($see.Contains("bench_alpha") -and $see.Contains("bench_bravo") -and $see.Contains("bench_charlie"))
  } finally {
    (New-Object -ComObject Shell.Application).Windows() | Where-Object { $_.LocationURL -like "*$leaf*" } | ForEach-Object { $_.Quit() }
    Start-Sleep -Milliseconds 300
    Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue
  }
}

$tasks["wait-change"] = {
  param($mcp)
  $extra = @'
$d = New-Object System.Windows.Forms.Timer
$d.Interval = 1500
$d.Add_Tick({ $lbl.Text = "Status: Done"; $d.Stop() })
$d.Start()
'@
  $form = Start-TestForm "EYRA Bench Wait" $extra
  try {
    $hw = $null
    for ($i = 0; $i -lt 50 -and -not $hw; $i++) {
      Start-Sleep -Milliseconds 100
      $w = $mcp.Send("tools/call", @{ name = "list_windows"; arguments = @{} }).result.content[0].text
      $m = [regex]::Match($w, '(0x[0-9A-F]+) \| [^|]+\| "EYRA Bench Wait"')
      if ($m.Success) { $hw = $m.Groups[1].Value }
    }
    if (-not $hw) { throw "form did not appear" }
    $mcp.Tool("see", @{ window = $hw; coords = "center" }) | Out-Null
    $mcp.Tool("wait_change", @{ window = $hw; timeout_ms = 6000 }) | Out-Null
    Start-Sleep -Milliseconds 150
    $diff = $mcp.Tool("see_diff", @{ window = $hw; coords = "center" })
    return $diff.Contains("Status: Done")
  } finally { Stop-Process -Id $form.Id -Force -ErrorAction SilentlyContinue }
}

# ------------------------------------------------------------------ run
function Median([double[]]$v) { $s = $v | Sort-Object; if ($s.Count -eq 0) { return 0 }; return $s[[int][math]::Floor(($s.Count - 1) / 2)] }

$inv = [System.Globalization.CultureInfo]::InvariantCulture
$rows = @()
foreach ($name in @($tasks.Keys)) {
  if ($Only.Count -and $name -notin $Only) { continue }
  $ok = 0; $calls = @(); $tok = @(); $shot = @(); $ms = @(); $errors = @()
  for ($r = 1; $r -le $Runs; $r++) {
    $mcp = [Mcp]::new($Eyra)
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    try { if (& $tasks[$name] $mcp) { $ok++ } else { $errors += "check failed" } }
    catch { $errors += $_.Exception.Message }
    finally { $sw.Stop(); $mcp.Close() }
    $calls += $mcp.Calls; $tok += [math]::Ceiling($mcp.Chars / 4); $shot += $mcp.ShotTokens; $ms += $sw.ElapsedMilliseconds
    Write-Host ("{0,-14} run {1}/{2}  ok={3}  calls={4}  tokens={5}  ms={6}" -f $name, $r, $Runs, $ok, $mcp.Calls, [math]::Ceiling($mcp.Chars / 4), $sw.ElapsedMilliseconds)
    Start-Sleep -Milliseconds 400
  }
  $mt = Median $tok; $mshot = Median $shot
  $rows += [pscustomobject]@{
    Task = $name; Success = "$ok/$Runs"; Calls = Median $calls; Tokens = $mt
    ScreenshotTokens = $mshot
    Saving = if ($mshot -gt 0) { [string]::Format($inv, "{0:N0}%", 100 * (1 - $mt / $mshot)) } else { "-" }
    Seconds = [string]::Format($inv, "{0:N1}", (Median $ms) / 1000)
    Errors = ($errors | Select-Object -Unique) -join "; "
  }
}

# ------------------------------------------------------------------ report
$os = Get-CimInstance Win32_OperatingSystem
$cpu = (Get-CimInstance Win32_Processor | Select-Object -First 1).Name.Trim()
Add-Type -AssemblyName System.Windows.Forms
$scr = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
$version = (& $Eyra --version).Trim()
$md = @(
  "# EYRA real-task benchmark"
  ""
  "Generated by ``bench/run-tasks.ps1`` on $(Get-Date -Format 'yyyy-MM-dd'): $version, $($os.Caption) (build $($os.BuildNumber)), $($scr.Width)x$($scr.Height), $cpu. $Runs runs per task; medians shown."
  ""
  "The steps are scripted (no LLM in the loop), so **Success** measures whether EYRA's observations and actions work reliably. **Tokens** are all tool output an agent would read (chars/4). **Screenshot tokens** are what the same observations would cost as full-window screenshots (w*h/750 each); action calls are the same either way."
  ""
  "| Task | Success | Tool calls | Tokens (EYRA) | Tokens (screenshots) | Saving | Time (s) |"
  "|---|---|---|---|---|---|---|"
)
foreach ($r in $rows) { $md += "| $($r.Task) | $($r.Success) | $($r.Calls) | $($r.Tokens) | $($r.ScreenshotTokens) | $($r.Saving) | $($r.Seconds) |" }
$failed = $rows | Where-Object { $_.Errors }
if ($failed) { $md += ""; $md += "Failures:"; foreach ($f in $failed) { $md += "- $($f.Task): $($f.Errors)" } }
$md -join "`n" | Set-Content -Encoding UTF8 $Out
Write-Host "`nWrote $Out"
$rows | Format-Table Task, Success, Calls, Tokens, ScreenshotTokens, Saving, Seconds -AutoSize
