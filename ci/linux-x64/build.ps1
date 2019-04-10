param (
  [string] $shortver = "99.99.99"
)

$ErrorActionPreference = "Stop"
Push-Location "$PSScriptRoot/../../"

. "./ci/build-deps.ps1"

Write-Host $env:PATH

ls /home/appveyor/.cargo/bin

dotnet --version
rustc --version

Initialize-Filesystem
Invoke-LinuxBuild
Invoke-DockerBuild

Build-TestAppContainer
Start-SeqEnvironment
Invoke-TestApp
Check-SqelfLogs
Check-SeqLogs
Check-ClefOutput
Stop-SeqEnvironment

if ($IsPublishedBuild) {
    Publish-Container $shortver
}
else {
    Write-Output "Not publishing Docker container"
}

Pop-Location