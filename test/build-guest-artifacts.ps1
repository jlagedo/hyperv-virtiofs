<#
.SYNOPSIS
  Build the Rocky Linux guest artifacts (vmlinuz + initramfs.cpio.gz) the end-to-end
  test ladder boots. See docs/testing.md.

.DESCRIPTION
  The actual build runs in a Docker container driven by test/guest/build-rocky-initramfs.sh.
  Because that script needs a Linux shell + the docker CLI, this wrapper invokes it through
  WSL. Requirements:
    * WSL2 with a Linux distro installed (`wsl --install`), and
    * Docker reachable from inside WSL — either Docker Desktop with WSL integration enabled,
      or Docker Engine installed in the distro.

  Output lands in test/guest/out/ (git-ignored):
    out/vmlinuz             ~16 MB  stock Rocky kernel
    out/initramfs.cpio.gz   ~500 MB full Rocky rootfs + the /init self-test

  The build pulls the Rocky image and dnf-installs a kernel, so the first run can take
  several minutes and needs network access. Re-run after editing test/guest/init.

.PARAMETER Distro
  WSL distro to run the build in. Defaults to the WSL default distro.

.PARAMETER Image
  Rocky container image to harvest the kernel + modules from. Default rockylinux/rockylinux:10.

.EXAMPLE
  .\test\build-guest-artifacts.ps1
.EXAMPLE
  .\test\build-guest-artifacts.ps1 -Distro Ubuntu
#>
[CmdletBinding()]
param(
  [string]$Distro,
  [string]$Image = "rockylinux/rockylinux:10"
)
$ErrorActionPreference = "Stop"

$scriptDir = $PSScriptRoot
$buildSh   = Join-Path $scriptDir "guest\build-rocky-initramfs.sh"
$outDir    = Join-Path $scriptDir "guest\out"
if (-not (Test-Path $buildSh)) { throw "build script not found: $buildSh" }

# Locate wsl.exe.
if (-not (Get-Command wsl.exe -ErrorAction SilentlyContinue)) {
  throw "wsl.exe not found. Install WSL2 (`wsl --install`) and a Linux distro, or build the artifacts manually per docs/testing.md."
}

$distroArgs = @()
if ($Distro) { $distroArgs = @("-d", $Distro) }

# Translate the Windows script path to a WSL path (e.g. E:\dev\x -> /mnt/e/dev/x).
# Done in PowerShell rather than via `wsl wslpath` because passing a backslash path
# through wsl.exe's argument parser strips the separators.
function ConvertTo-WslPath([string]$winPath) {
  $full = (Resolve-Path -LiteralPath $winPath).Path
  if ($full -notmatch '^[A-Za-z]:\\') { throw "expected a drive-letter path: $full" }
  $drive = $full.Substring(0, 1).ToLower()
  $rest  = ($full.Substring(2) -replace '\\', '/')
  return "/mnt/$drive$rest"
}
$wslBuildSh = ConvertTo-WslPath $buildSh
if (-not $wslBuildSh) { throw "could not translate path to WSL: $buildSh" }

# Sanity-check docker is reachable inside WSL before the long build.
& wsl.exe @distroArgs docker version --format '{{.Server.Version}}' 2>$null | Out-Null
if ($LASTEXITCODE -ne 0) {
  throw "docker is not reachable inside WSL. Enable Docker Desktop's WSL integration for this distro, or install Docker Engine in it. See docs/testing.md."
}

Write-Host "Building guest artifacts via WSL ($(if ($Distro) {$Distro} else {'default distro'}))..." -ForegroundColor Cyan
Write-Host "  script: $wslBuildSh"
Write-Host "  image:  $Image"

# HVFS_ROCKY_IMAGE is read by the bash script; pass it through the WSL env.
& wsl.exe @distroArgs --exec env HVFS_ROCKY_IMAGE="$Image" bash "$wslBuildSh"
if ($LASTEXITCODE -ne 0) { throw "guest artifact build failed (exit $LASTEXITCODE)" }

$kernel = Join-Path $outDir "vmlinuz"
$initrd = Join-Path $outDir "initramfs.cpio.gz"
foreach ($f in @($kernel, $initrd)) {
  if (-not (Test-Path $f)) { throw "expected artifact missing after build: $f" }
}
Write-Host "`nArtifacts ready:" -ForegroundColor Green
Get-Item $kernel, $initrd | Format-Table Name, @{N='Size(MB)';E={[math]::Round($_.Length/1MB,1)}}, FullName -AutoSize
Write-Host "Run the suite with:  .\test\run-e2e.ps1" -ForegroundColor Cyan
