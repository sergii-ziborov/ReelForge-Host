# Run Host Criterion benches and print the HTML report path.
Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
Set-Location (Split-Path -Parent $PSScriptRoot)
cargo bench --bench host
Write-Host "HTML report: $PWD\target\criterion\report\index.html"
