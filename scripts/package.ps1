<#
.SYNOPSIS
  Builds all EYRA release assets into .\dist:
    eyra.exe                          standalone binary
    eyra-<ver>-windows-x64.zip        exe + docs + license
    eyra-<ver>.mcpb                   Claude Desktop one-click extension (MCP Bundle)
    win32.x64.eyra.zip                Gemini CLI extension archive
    SHA256SUMS.txt                    checksums (LF line endings)
  The Claude Code plugin downloads eyra-<ver>.mcpb from the GitHub release, so no binary lives
  in the repository. Releases are built by .github/workflows/release.yml.

.NOTES
  Requires: cargo (Rust) and Node.js (npx downloads the official @anthropic-ai/mcpb CLI).
  Usage:    powershell -ExecutionPolicy Bypass -File scripts\package.ps1 [-SkipBuild]
#>
param([switch]$SkipBuild)
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

$version = (Select-String -Path Cargo.toml -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
Write-Host "EYRA $version"

# Rust embeds absolute source paths (for panic/error messages) in the binary. Remap the
# machine-specific prefixes so no user name or home folder ends up in a published exe.
# rustc applies the LAST matching remap, so the generic home folder goes first.
$cargoHome  = if ($env:CARGO_HOME)  { $env:CARGO_HOME }  else { Join-Path $env:USERPROFILE ".cargo" }
$rustupHome = if ($env:RUSTUP_HOME) { $env:RUSTUP_HOME } else { Join-Path $env:USERPROFILE ".rustup" }
$remap = @(
  "--remap-path-prefix=$($env:USERPROFILE)=/home",
  "--remap-path-prefix=$cargoHome=/cargo",
  "--remap-path-prefix=$rustupHome=/rustup",
  "--remap-path-prefix=$root=/eyra"
)
# CARGO_ENCODED_RUSTFLAGS uses 0x1F as separator, so paths with spaces stay intact
$env:CARGO_ENCODED_RUSTFLAGS = ($remap -join [char]0x1F)

# Separate target dir: a normal `cargo build/test` (without the remap) can never overwrite
# the binary this script packages.
$env:CARGO_TARGET_DIR = Join-Path $root "target\package"
if (-not $SkipBuild) { cargo build --release; if ($LASTEXITCODE -ne 0) { throw "cargo build failed" } }
$exe = Join-Path $env:CARGO_TARGET_DIR "release\eyra.exe"
if (-not (Test-Path $exe)) { throw "missing $exe" }

# Refuse to package a binary that still carries the builder's user name or home path
$bytes = [IO.File]::ReadAllBytes($exe)
$asText = @([Text.Encoding]::ASCII.GetString($bytes), [Text.Encoding]::Unicode.GetString($bytes))
foreach ($needle in @($env:USERPROFILE, $env:USERNAME) | Where-Object { $_ -and $_.Length -ge 3 }) {
  foreach ($t in $asText) {
    if ($t.IndexOf($needle, [StringComparison]::OrdinalIgnoreCase) -ge 0) {
      throw "eyra.exe still contains the local path or user name '$needle'. Build with this script (or in CI), not plain cargo."
    }
  }
}
Write-Host "Privacy check passed: no local user paths in eyra.exe"

# Keep the manifests' versions in sync with Cargo.toml
foreach ($m in @("packaging\mcpb\manifest.json", "gemini-extension.json", "claude-plugin\.claude-plugin\plugin.json")) {
  $json = Get-Content $m -Raw | ConvertFrom-Json
  if ($json.version -ne $version) { throw "$m has version $($json.version), Cargo.toml has $version" }
}

# The Claude plugin installs the bundle from this version's release
$pluginServers = (Get-Content "claude-plugin\.claude-plugin\plugin.json" -Raw | ConvertFrom-Json).mcpServers
$expectedBundle = "https://github.com/layer8labs8/EYRA/releases/download/v$version/eyra-$version.mcpb"
if ($pluginServers -ne $expectedBundle) { throw "plugin.json mcpServers should be $expectedBundle" }

$dist = Join-Path $root "dist"
if (Test-Path $dist) { Remove-Item -Recurse -Force $dist }
New-Item -ItemType Directory $dist | Out-Null
$stage = Join-Path $dist "stage"

# 1. Standalone exe
Copy-Item $exe $dist

# 2. Plain zip
$plain = Join-Path $stage "eyra-$version-windows-x64"
New-Item -ItemType Directory -Force $plain | Out-Null
Copy-Item $exe, README.md, LICENSE, SKILL.md, mcp-config.example.json $plain
Compress-Archive -Path $plain -DestinationPath (Join-Path $dist "eyra-$version-windows-x64.zip")

# 3. Claude Desktop MCP Bundle
$mcpb = Join-Path $stage "mcpb"
New-Item -ItemType Directory -Force (Join-Path $mcpb "server") | Out-Null
Copy-Item packaging\mcpb\manifest.json $mcpb
Copy-Item assets\eyra-icon.png (Join-Path $mcpb "icon.png")
Copy-Item LICENSE $mcpb
Copy-Item $exe (Join-Path $mcpb "server")
npx --yes @anthropic-ai/mcpb@2.1.2 pack $mcpb (Join-Path $dist "eyra-$version.mcpb")
if ($LASTEXITCODE -ne 0) { throw "mcpb pack failed" }

# 4. Gemini CLI extension (manifest must sit at the archive root)
$gem = Join-Path $stage "gemini"
New-Item -ItemType Directory -Force $gem | Out-Null
Copy-Item gemini-extension.json, GEMINI.md, LICENSE, $exe $gem
Compress-Archive -Path (Join-Path $gem "*") -DestinationPath (Join-Path $dist "win32.x64.eyra.zip")

Remove-Item -Recurse -Force $stage

# 5. Checksums with LF endings so `sha256sum -c` works everywhere
$lines = Get-ChildItem $dist -File | Sort-Object Name | ForEach-Object {
  "{0} *{1}" -f (Get-FileHash $_.FullName -Algorithm SHA256).Hash.ToLower(), $_.Name
}
[IO.File]::WriteAllText((Join-Path $dist "SHA256SUMS.txt"), ($lines -join "`n") + "`n")

Get-ChildItem $dist | Format-Table Name, Length -AutoSize
