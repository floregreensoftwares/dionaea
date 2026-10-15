Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$repoRoot = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
& git -C $repoRoot config core.hooksPath .githooks
& git -C $repoRoot config commit.template .gitmessage

Write-Host "Configured dionaea hooks path as .githooks and commit template as .gitmessage"
