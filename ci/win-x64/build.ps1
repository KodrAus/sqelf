param (
  [string] $shortver = "99.99.99"
)

$ErrorActionPreference = "Stop"
Push-Location "$PSScriptRoot/../../"

. "./ci/build-deps.ps1"

Initialize-Docker
Initialize-Filesystem
Invoke-WindowsBuild
Invoke-NuGetPack $shortver

Pop-Location