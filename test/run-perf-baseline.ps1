<#
.SYNOPSIS
  Run the virtio-fs performance baseline (perf_baseline.rs + guest run_perfbench) on a
  live Hyper-V VM and emit target/perf-baseline/baseline.{md,json}.

.DESCRIPTION
  This is the perf counterpart to run-e2e.ps1. It:
    1. Ensures the perfbench initramfs exists (out/initramfs.perfbench.cpio.gz) — a repack
       of the stock initramfs with the current test/guest/init (which carries run_perfbench).
       Repacking is done in WSL (cpio/gzip, no Docker); pass -Rebuild to force it.
    2. Points HVFS_INITRD at it, optionally enables host aperture-cache stats, and runs the
       perf_baseline test (repeated boots; medians).
    3. Tees the full console to target/perf-baseline/run-<stamp>.log and, when -ApertureStats
       is set, summarises the host-side `aperture stats` lines.

  Requires Hyper-V + WSL (for the one-time repack). See docs/testing.md.

.PARAMETER Seqmb     Sequential file size MiB (guest atelier.pb_seqmb). Default 64.
.PARAMETER Meta      Metadata file count (guest atelier.pb_meta). Default 1000.
.PARAMETER Rand      Random 4k read count (guest atelier.pb_rand). Default 300.
.PARAMETER Jobs      Parallel guest jobs for the PB_PAR_* phases (atelier.pb_jobs). Default 8.
.PARAMETER Repeats   Benchmark runs to median over (HVFS_PB_REPEATS). Default 3.
.PARAMETER ApertureStats  Enable VIRTIO_HDV_APERTURE_STATS=1 (host aperture-cache stats).
.PARAMETER Rebuild   Force-rebuild the perfbench initramfs from test/guest/init.
.PARAMETER Distro    WSL distro for the repack. Default: WSL default distro.
.PARAMETER WorkspaceDir  Share backing dir (the disk under test). Default: repo-local
                         target/perf-share (same drive as the repo), NOT %TEMP%.

.EXAMPLE
  .\test\run-perf-baseline.ps1
.EXAMPLE
  .\test\run-perf-baseline.ps1 -Seqmb 128 -Repeats 5 -ApertureStats
#>
[CmdletBinding()]
param(
  [int]$Seqmb = 64,
  [int]$Meta = 1000,
  [int]$Rand = 300,
  [int]$Jobs = 8,
  [int]$Repeats = 3,
  [switch]$ApertureStats,
  [switch]$Rebuild,
  [string]$Distro,
  [string]$WorkspaceDir
)
$ErrorActionPreference = "Stop"
$repo = Split-Path $PSScriptRoot -Parent
$outDir = Join-Path $repo "test\guest\out"
$stockInitrd = Join-Path $outDir "initramfs.cpio.gz"
$perfInitrd  = Join-Path $outDir "initramfs.perfbench.cpio.gz"
$kernel      = Join-Path $outDir "vmlinuz"
$initScript  = Join-Path $repo "test\guest\init"
$reportDir   = Join-Path $repo "target\perf-baseline"

# Resolve protoc the workspace build needs.
if (-not $env:PROTOC) {
  $p = Join-Path $repo "..\tools\protoc\bin\protoc.exe"
  if (Test-Path $p) { $env:PROTOC = (Resolve-Path $p).Path }
}

foreach ($f in @($stockInitrd, $kernel)) {
  if (-not (Test-Path $f)) { throw "missing guest artifact: $f (run test\build-guest-artifacts.ps1 first)" }
}

function ConvertTo-WslPath([string]$winPath) {
  $full = (Resolve-Path -LiteralPath $winPath).Path
  $drive = $full.Substring(0, 1).ToLower()
  return "/mnt/$drive$(($full.Substring(2)) -replace '\\','/')"
}

# Repack the stock initramfs with the current init, in WSL, as root (preserves perms).
function Build-PerfInitrd {
  Write-Host "Repacking perfbench initramfs via WSL (no Docker)..." -ForegroundColor Cyan
  if (-not (Get-Command wsl.exe -ErrorAction SilentlyContinue)) { throw "wsl.exe not found" }
  $distroArgs = @(); if ($Distro) { $distroArgs = @("-d", $Distro) }
  $repackWsl = ConvertTo-WslPath (Join-Path $repo "test\guest\repack-perfbench-initramfs.sh")
  # Run as root (WSL -u root needs no password; sudo would prompt and hang). The script
  # file avoids cross-boundary shell-quoting pitfalls with inline scripts.
  & wsl.exe @distroArgs -u root -- bash $repackWsl
  if ($LASTEXITCODE -ne 0) { throw "perfbench initramfs repack failed (exit $LASTEXITCODE)" }
}

# (Re)build the perfbench initramfs if missing or older than the init script.
$needBuild = $Rebuild -or (-not (Test-Path $perfInitrd)) -or `
  ((Get-Item $initScript).LastWriteTime -gt (Get-Item $perfInitrd -ErrorAction SilentlyContinue).LastWriteTime)
if ($needBuild) { Build-PerfInitrd } else { Write-Host "perfbench initramfs up to date: $perfInitrd" -ForegroundColor Green }

New-Item -ItemType Directory -Force -Path $reportDir | Out-Null
$stamp = Get-Date -Format "yyyyMMdd-HHmmss"
$log = Join-Path $reportDir "run-$stamp.log"

# Env for the test run.
$env:HVFS_KERNEL = (Resolve-Path $kernel).Path
$env:HVFS_INITRD = (Resolve-Path $perfInitrd).Path
$env:HVFS_PB_SEQMB = "$Seqmb"
$env:HVFS_PB_META = "$Meta"
$env:HVFS_PB_RAND = "$Rand"
$env:HVFS_PB_JOBS = "$Jobs"
$env:HVFS_PB_REPEATS = "$Repeats"
if ($WorkspaceDir) {
  New-Item -ItemType Directory -Force $WorkspaceDir | Out-Null
  $env:HVFS_PB_WS = (Resolve-Path $WorkspaceDir).Path
  Write-Host "  share workspace (backing disk under test): $($env:HVFS_PB_WS)"
} else {
  Remove-Item Env:\HVFS_PB_WS -ErrorAction SilentlyContinue
}
if ($ApertureStats) { $env:VIRTIO_HDV_APERTURE_STATS = "1" } else { Remove-Item Env:\VIRTIO_HDV_APERTURE_STATS -ErrorAction SilentlyContinue }

Write-Host "Running perf baseline (seqmb=$Seqmb meta=$Meta rand=$Rand jobs=$Jobs repeats=$Repeats apertureStats=$ApertureStats workers=$env:VIRTIO_HDV_WORKERS)" -ForegroundColor Cyan
Write-Host "  log: $log"

# --nocapture so guest console + (optional) aperture stats reach the log.
cargo test -p hcs-testvm --release --test perf_baseline -- --ignored --nocapture --exact perf_baseline_over_virtiofs 2>&1 |
  Tee-Object -FilePath $log
$code = $LASTEXITCODE

if ($ApertureStats) {
  Write-Host "`n--- host aperture-cache stats (final per run) ---" -ForegroundColor Cyan
  Select-String -Path $log -Pattern "aperture stats|phase=final|hits=|creates=|evicts=|quota_retries=" |
    ForEach-Object { $_.Line.Trim() } | Select-Object -Last 20
}

$md = Join-Path $reportDir "baseline.md"
if (Test-Path $md) {
  Write-Host "`n===== baseline.md =====" -ForegroundColor Green
  Get-Content $md
  Write-Host "`nReport: $md" -ForegroundColor Green
} else {
  Write-Host "No baseline.md produced — check $log" -ForegroundColor Yellow
}
exit $code
