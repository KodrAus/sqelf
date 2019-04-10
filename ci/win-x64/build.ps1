param (
  [string] $shortver = "99.99.99"
)

$ErrorActionPreference = "Stop"
Push-Location "$PSScriptRoot/../../"

. "./ci/build-deps.ps1"

dotnet --version
rustc --version

Initialize-Filesystem
Invoke-WindowsBuild
Invoke-NuGetPack $shortver

Pop-Location