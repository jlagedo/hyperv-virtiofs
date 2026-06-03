<#
.SYNOPSIS
  Run the hyperv-virtiofs end-to-end test ladder against a live Hyper-V host.
  See docs/testing.md for the full guide.

.DESCRIPTION
  Each rung boots a throwaway Rocky Linux VM via HCS and drives one more layer of the
  stack (boot -> proxy transport -> cold virtio-fs mount -> live hot-add -> the public
  C ABI). These tests are #[ignore] in cargo because they need:
    * a Hyper-V-capable Windows host (Hyper-V / Windows Hypervisor Platform enabled), and
    * the guest artifacts in test\guest\out\ (build them with test\build-guest-artifacts.ps1).

  This script verifies the artifacts exist (building them if -Build is given), points
  HVFS_KERNEL / HVFS_INITRD at them, runs the ladder in order, and prints a PASS/FAIL
  summary. It exits non-zero if any rung fails.

  file_selftest is a *best-effort* test (sustained guest I/O gated by the HDV aperture
  coherence limitation); it is excluded from the green ladder unless you pass
  -IncludeBestEffort. The two negative spikes (attach, attach_oop) reproduce a platform
  limitation and *assert success*, so they fail by design; excluded unless -IncludeNegativeSpikes.

.PARAMETER Test
  Run only this test target (e.g. boot, attach_virtiofs, hotplug, attach_abi, file_selftest).

.PARAMETER Build
  Build the guest artifacts first (via test\build-guest-artifacts.ps1), even if present.

.PARAMETER IncludeBestEffort
  Also run file_selftest (sustained I/O; may fail under the aperture-coherence limitation).

.PARAMETER IncludeNegativeSpikes
  Also run attach + attach_oop (expected to fail on current Windows; see docs/testing.md).

.PARAMETER List
  List the ladder and exit.

.EXAMPLE
  .\test\run-e2e.ps1
.EXAMPLE
  .\test\run-e2e.ps1 -Test attach_abi
.EXAMPLE
  .\test\run-e2e.ps1 -Build
#>
[CmdletBinding()]
param(
  [string]$Test,
  [switch]$Build,
  [switch]$IncludeBestEffort,
  [switch]$IncludeNegativeSpikes,
  [switch]$List
)
$ErrorActionPreference = "Stop"

# The green ladder, in dependency order. Each entry: cargo --test target + what it proves.
$ladder = @(
  @{ Name = "boot";            Proves = "rig works: Rocky boots under HCS to userspace" }
  @{ Name = "attach_proxy";    Proves = "ExternalRestricted FlexibleIov proxy transport" }
  @{ Name = "attach_virtiofs"; Proves = "guest mounts virtio-fs over HDV (device pre-start)" }
  @{ Name = "hotplug";         Proves = "live hot-add: 1 device, 2 concurrent, cold-declared" }
  @{ Name = "attach_abi";      Proves = "the shipped C ABI v2 end-to-end (authoritative)" }
  @{ Name = "edge_cases";      Proves = "C ABI rejects bad share input (ro/guid/json) against a live host" }
  @{ Name = "concurrent_processes"; Proves = "Model A: independent device hosts in separate processes mount concurrently" }
)
# Best-effort: exercises sustained guest I/O, which is gated by the unfixed HDV aperture
# eviction/coherence limitation (docs/share-abi.md, roadmap.md). Passes on a good boot but
# isn't a reliable gate, so it is NOT part of the green ladder. Run with -IncludeBestEffort
# or -Test file_selftest.
$bestEffort = @(
  @{ Name = "file_selftest"; Proves = "[best-effort] file integrity (host sha256), throughput, many-files, unicode names" }
)
$negativeSpikes = @(
  @{ Name = "attach";     Proves = "[expected FAIL] in-process attach without proxy" }
  @{ Name = "attach_oop"; Proves = "[expected FAIL] out-of-process attach without HCS-launched emulator" }
)

if ($List) {
  Write-Host "End-to-end ladder (run in order):`n" -ForegroundColor Cyan
  $ladder | ForEach-Object { "{0,-20} {1}" -f $_.Name, $_.Proves }
  Write-Host "`nBest-effort (-IncludeBestEffort; gated by the aperture-coherence limitation):" -ForegroundColor Yellow
  $bestEffort | ForEach-Object { "{0,-20} {1}" -f $_.Name, $_.Proves }
  Write-Host "`nNegative spikes (-IncludeNegativeSpikes; expected to fail by design):" -ForegroundColor Yellow
  $negativeSpikes | ForEach-Object { "{0,-20} {1}" -f $_.Name, $_.Proves }
  return
}

$repoRoot = Split-Path -Parent $PSScriptRoot
$outDir   = Join-Path $PSScriptRoot "guest\out"
$kernel   = Join-Path $outDir "vmlinuz"
$initrd   = Join-Path $outDir "initramfs.cpio.gz"

# Build artifacts if asked, or if they are missing.
if ($Build -or -not (Test-Path $kernel) -or -not (Test-Path $initrd)) {
  if (-not $Build) {
    Write-Host "Guest artifacts not found in $outDir — building them now..." -ForegroundColor Yellow
  }
  & (Join-Path $PSScriptRoot "build-guest-artifacts.ps1")
}
foreach ($f in @($kernel, $initrd)) {
  if (-not (Test-Path $f)) { throw "guest artifact missing: $f. Build it with test\build-guest-artifacts.ps1." }
}

# Point the tests at the in-repo artifacts (absolute paths; the tests also default here).
$env:HVFS_KERNEL = (Resolve-Path $kernel).Path
$env:HVFS_INITRD = (Resolve-Path $initrd).Path
Write-Host "HVFS_KERNEL = $($env:HVFS_KERNEL)"
Write-Host "HVFS_INITRD = $($env:HVFS_INITRD)`n"

# Select which targets to run.
$targets = $ladder
if ($IncludeBestEffort) { $targets = $targets + $bestEffort }
if ($IncludeNegativeSpikes) { $targets = $targets + $negativeSpikes }
if ($Test) {
  $targets = ($ladder + $bestEffort + $negativeSpikes) | Where-Object { $_.Name -eq $Test }
  if (-not $targets) { throw "unknown test '$Test'. Use -List to see the ladder." }
}

$results = @()
foreach ($t in $targets) {
  $name = $t.Name
  Write-Host ("=" * 78) -ForegroundColor DarkGray
  Write-Host "RUN  $name  —  $($t.Proves)" -ForegroundColor Cyan
  Write-Host ("=" * 78) -ForegroundColor DarkGray
  $sw = [System.Diagnostics.Stopwatch]::StartNew()
  Push-Location $repoRoot
  try {
    # --test-threads=1 is required: each e2e test boots a VM and registers an HDV
    # device host, and the platform allows only one such registration at a time —
    # running a target's tests in parallel (e.g. hotplug's three) collides
    # (from_proxy -> E_ACCESSDENIED) and can crash on teardown.
    & cargo test -p hcs-testvm --test $name -- --ignored --nocapture --test-threads=1
    $code = $LASTEXITCODE
  } finally {
    Pop-Location
  }
  $sw.Stop()
  $results += [pscustomobject]@{
    Test    = $name
    Result  = if ($code -eq 0) { "PASS" } else { "FAIL" }
    Seconds = [math]::Round($sw.Elapsed.TotalSeconds, 1)
  }
}

Write-Host "`n$('=' * 78)" -ForegroundColor DarkGray
Write-Host "SUMMARY" -ForegroundColor Cyan
$results | Format-Table Test, Result, Seconds -AutoSize | Out-String | Write-Host
$failed = @($results | Where-Object { $_.Result -ne "PASS" })
if ($failed.Count -gt 0) {
  Write-Host "$($failed.Count) of $($results.Count) failed." -ForegroundColor Red
  exit 1
}
Write-Host "All $($results.Count) passed." -ForegroundColor Green
