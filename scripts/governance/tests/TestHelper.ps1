# TestHelper.ps1
# Test helper library for Palka Governance Engine Self-Tests (DEC-003 Phase 2A R1)
# FOR SELF-TEST HARNESS ONLY - NOT USED BY PRODUCTION ENGINE

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function New-TestGitRepo {
    [CmdletBinding()]
    param (
        [string]$Prefix = 'palka-test-repo',
        [switch]$WithBareOrigin
    )

    $guid = [Guid]::NewGuid().ToString('N')
    $repoDir = Join-Path $env:TEMP "$Prefix-$guid"
    New-Item -ItemType Directory -Force -Path $repoDir | Out-Null

    # Initialize git repository
    $proc = New-Object System.Diagnostics.Process
    $proc.StartInfo.FileName = 'git'
    $proc.StartInfo.WorkingDirectory = $repoDir
    $proc.StartInfo.UseShellExecute = $false
    $proc.StartInfo.RedirectStandardOutput = $true
    $proc.StartInfo.RedirectStandardError = $true
    $proc.StartInfo.CreateNoWindow = $true

    $proc.StartInfo.Arguments = 'init -b main'
    $proc.Start() | Out-Null
    $proc.WaitForExit()

    $proc.StartInfo.Arguments = 'config user.name "Palka Test"'
    $proc.Start() | Out-Null
    $proc.WaitForExit()

    $proc.StartInfo.Arguments = 'config user.email "test@palka.local"'
    $proc.Start() | Out-Null
    $proc.WaitForExit()

    # Create initial file and commit
    $initFile = Join-Path $repoDir 'README.md'
    [System.IO.File]::WriteAllText($initFile, "# Test Repo`n", [System.Text.UTF8Encoding]::new($false))

    $proc.StartInfo.Arguments = 'add README.md'
    $proc.Start() | Out-Null
    $proc.WaitForExit()

    $proc.StartInfo.Arguments = 'commit -m "initial commit"'
    $proc.Start() | Out-Null
    $proc.WaitForExit()

    # Get HEAD SHA
    $proc.StartInfo.Arguments = 'rev-parse HEAD'
    $proc.Start() | Out-Null
    $headSha = $proc.StandardOutput.ReadToEnd().Trim()
    $proc.WaitForExit()

    $bareDir = $null
    if ($WithBareOrigin) {
        $bareDir = Join-Path $env:TEMP "$Prefix-bare-$guid.git"
        $proc.StartInfo.WorkingDirectory = $env:TEMP
        $proc.StartInfo.Arguments = "clone --bare `"$repoDir`" `"$bareDir`""
        $proc.Start() | Out-Null
        $proc.WaitForExit()

        $proc.StartInfo.WorkingDirectory = $repoDir
        $proc.StartInfo.Arguments = "remote add origin `"$bareDir`""
        $proc.Start() | Out-Null
        $proc.WaitForExit()

        $proc.StartInfo.Arguments = 'fetch origin'
        $proc.Start() | Out-Null
        $proc.WaitForExit()
    }

    return [PSCustomObject]@{
        RepoDir = $repoDir
        HeadSha = $headSha
        Branch = 'main'
        BareDir = $bareDir
    }
}

function Remove-TestGitRepo {
    [CmdletBinding()]
    param (
        [string]$RepoDir = $null,
        [string]$BareDir = $null
    )

    if ($null -ne $RepoDir -and $RepoDir.Length -gt 0 -and (Test-Path -LiteralPath $RepoDir)) {
        Remove-Item -LiteralPath $RepoDir -Recurse -Force -ErrorAction SilentlyContinue
    }
    if ($null -ne $BareDir -and $BareDir.Length -gt 0 -and (Test-Path -LiteralPath $BareDir)) {
        Remove-Item -LiteralPath $BareDir -Recurse -Force -ErrorAction SilentlyContinue
    }
}

function New-TestManifest {
    [CmdletBinding()]
    param (
        [Parameter(Mandatory = $true)]
        [string]$RepoDir,

        [Parameter(Mandatory = $true)]
        [string]$HeadSha,

        [string]$Branch = 'main',
        [string]$OperationId = 'TEST-OPERATION',
        [string]$Stage = 'IMPLEMENTATION',
        [hashtable]$CustomProperties = @{}
    )

    $manifest = [ordered]@{
        schema = 'palka.operation-manifest/v1'
        operation_id = $OperationId
        repository = 'jcmir/palka'
        working_directory = $RepoDir
        stage = $Stage
        branch = $Branch
        expected_start_branch = $Branch
        target_branch = $Branch
        expected_head = $HeadSha
        expected_base = $HeadSha
        expected_remote_refs = [ordered]@{}
        authorized_paths = @('**')
        forbidden_paths = @()
        branch_transition = [ordered]@{
            allowed = $false
        }
        refresh_commands = @()
        required_preconditions = @()
        already_satisfied_checks = @()
        authorized_commands = @()
        required_postconditions = @()
        artifact_profile = 'bootstrap_zip_v1'
        stop_conditions = @()
    }

    foreach ($k in $CustomProperties.Keys) {
        $manifest[$k] = $CustomProperties[$k]
    }

    $guid = [Guid]::NewGuid().ToString('N')
    $manifestPath = Join-Path $env:TEMP "manifest-$guid.json"
    $json = $manifest | ConvertTo-Json -Depth 10
    [System.IO.File]::WriteAllText($manifestPath, $json, [System.Text.UTF8Encoding]::new($false))

    return [PSCustomObject]@{
        ManifestPath = $manifestPath
        ManifestObject = ($json | ConvertFrom-Json)
    }
}
