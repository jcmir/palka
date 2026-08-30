# Invoke-PalkaOperation.ps1
# CLI entry point for Palka Governance Execution Engine (DEC-003 Phase 2A.2)

[CmdletBinding()]
param (
    [Parameter(Mandatory = $true)]
    [string]$ManifestPath,

    [Parameter(Mandatory = $true)]
    [string]$OutputRoot,

    [Parameter(Mandatory = $false)]
    [string]$AuthorizedManifestSha256 = $null,

    [switch]$PassThru
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

try {
    $scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
    $modulePath = Join-Path $scriptDir 'PalkaGovernance.psm1'

    Import-Module $modulePath -Force

    $summary = Invoke-PalkaEngine -ManifestPath $ManifestPath -OutputRoot $OutputRoot -AuthorizedManifestSha256 $AuthorizedManifestSha256 -PassThru

    Write-Host "RESULT: $($summary.result)"
    Write-Host "MUTATION_STATE: $($summary.mutation_state)"
    Write-Host "OPERATION_ID: $($summary.operation_id)"

    if ($null -ne $summary.run_directory -and (Test-Path -LiteralPath $summary.run_directory)) {
        Write-Host "RUN_DIRECTORY: $($summary.run_directory)"
        $expectedSummaryFile = Join-Path $summary.run_directory 'summary.json'
        if (Test-Path -LiteralPath $expectedSummaryFile) {
            Write-Host "SUMMARY: $expectedSummaryFile"
        }
        else {
            Write-Host "SUMMARY: <none>"
        }
    }
    else {
        Write-Host "RUN_DIRECTORY: <none>"
        Write-Host "SUMMARY: <none>"
    }

    if ($PassThru) {
        $summary
    }

    if ($summary.result -in @('COMPLETED', 'ALREADY_SATISFIED')) {
        exit 0
    }
    else {
        exit 2
    }
}
catch {
    Write-Host "RESULT: STOPPED"
    Write-Host "MUTATION_STATE: NOT_APPLIED"
    Write-Host "OPERATION_ID: INVALID-MANIFEST"
    Write-Host "RUN_DIRECTORY: <none>"
    Write-Host "SUMMARY: <none>"
    Write-Host "REASON: $($_.Exception.Message)"
    exit 2
}
