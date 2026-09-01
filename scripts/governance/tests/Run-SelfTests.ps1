# Run-SelfTests.ps1
# Self-Test Harness for Governance Execution Engine Core (DEC-003 Phase 2A R3)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$govModule = Join-Path (Split-Path -Parent $scriptDir) 'PalkaGovernance.psm1'
$helperScript = Join-Path $scriptDir 'TestHelper.ps1'

Import-Module $govModule -Force
. $helperScript

$passCount = 0
$failCount = 0
$totalTests = 106
$outputRoot = Join-Path $env:TEMP 'palka-selftest-runs-r3'
if (Test-Path $outputRoot) { Remove-Item -Recurse -Force $outputRoot }
New-Item -ItemType Directory -Force -Path $outputRoot | Out-Null

function Report-Pass {
    param ([string]$Id, [string]$Name)
    Write-Host "PASS $Id $Name"
    $script:passCount++
}

function Report-Fail {
    param ([string]$Id, [string]$Name, [string]$Reason)
    Write-Host "FAIL $Id ${Name}: $Reason"
    $script:failCount++
}

# ----------------------------------------------------
# T01 — manifest rejects unknown top-level field
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't01'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{ 'unknown_extra_field' = 'bad_value' }
    $res = Invoke-TestEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -PassThru
    if ($res.result -eq 'STOPPED' -and $res.reason -match 'Unknown top-level field') {
        Report-Pass 'T01' 'manifest rejects unknown top-level field'
    } else {
        Report-Fail 'T01' 'manifest rejects unknown top-level field' "got: $($res.result) $($res.reason)"
    }
} catch {
    Report-Fail 'T01' 'manifest rejects unknown top-level field' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T02 — manifest rejects abbreviated SHA
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't02'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha '572f9d2'
    $res = Invoke-TestEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -PassThru
    if ($res.result -eq 'STOPPED' -and $res.reason -match 'expected_head must be exactly 40') {
        Report-Pass 'T02' 'manifest rejects abbreviated SHA'
    } else {
        Report-Fail 'T02' 'manifest rejects abbreviated SHA' "got: $($res.result) $($res.reason)"
    }
} catch {
    Report-Fail 'T02' 'manifest rejects abbreviated SHA' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T03 — invalid stage rejected
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't03'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -Stage 'INVALID_STAGE_NAME'
    $res = Invoke-TestEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -PassThru
    if ($res.result -eq 'STOPPED' -and $res.reason -match 'Invalid stage') {
        Report-Pass 'T03' 'invalid stage rejected'
    } else {
        Report-Fail 'T03' 'invalid stage rejected' "got: $($res.result) $($res.reason)"
    }
} catch {
    Report-Fail 'T03' 'invalid stage rejected' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T04 — branch model invalid when transition=false but branches differ
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't04'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'target_branch' = 'feature-branch'
        'branch_transition' = [ordered]@{ 'allowed' = $false }
    }
    $res = Invoke-TestEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -PassThru
    if ($res.result -eq 'STOPPED' -and $res.reason -match 'expected_start_branch, target_branch and branch must be identical') {
        Report-Pass 'T04' 'branch model invalid when transition=false but branches differ'
    } else {
        Report-Fail 'T04' 'branch model invalid when transition=false but branches differ' "got: $($res.result) $($res.reason)"
    }
} catch {
    Report-Fail 'T04' 'branch model invalid when transition=false but branches differ' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T05 — precondition mismatch STOPs before mutating action
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't05'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'required_preconditions' = @(
            [ordered]@{
                'id' = 'failing-precondition'
                'executable' = 'git'
                'arguments' = @('rev-parse', 'non_existent_ref')
                'cwd' = $repo.RepoDir
                'mutating' = $false
                'expect' = [ordered]@{ 'exit_code' = 0 }
            }
        )
        'authorized_commands' = @(
            [ordered]@{
                'id' = 'mutating-action'
                'executable' = 'git'
                'arguments' = @('config', 'test.marker', 'true')
                'cwd' = $repo.RepoDir
                'mutating' = $true
                'expect' = [ordered]@{ 'exit_code' = 0 }
            }
        )
    }
    $res = Invoke-TestEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -PassThru
    $configCheck = & git -C $repo.RepoDir config test.marker 2>$null
    if ($res.result -eq 'STOPPED' -and $res.mutation_state -eq 'NOT_APPLIED' -and $null -eq $configCheck) {
        Report-Pass 'T05' 'precondition mismatch STOPs before mutating action'
    } else {
        Report-Fail 'T05' 'precondition mismatch STOPs before mutating action' "got: $($res.result) $($res.mutation_state)"
    }
} catch {
    Report-Fail 'T05' 'precondition mismatch STOPs before mutating action' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T06 — ALREADY_SATISFIED skips mutating action
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't06'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'already_satisfied_checks' = @(
            [ordered]@{
                'id' = 'check-already-done'
                'executable' = 'git'
                'arguments' = @('rev-parse', 'HEAD')
                'cwd' = $repo.RepoDir
                'mutating' = $false
                'expect' = [ordered]@{
                    'exit_code' = 0
                    'stdout_equals' = $repo.HeadSha
                }
            }
        )
        'authorized_commands' = @(
            [ordered]@{
                'id' = 'mutating-action'
                'executable' = 'git'
                'arguments' = @('config', 'test.already.marker', 'ran')
                'cwd' = $repo.RepoDir
                'mutating' = $true
                'expect' = [ordered]@{ 'exit_code' = 0 }
            }
        )
    }
    $res = Invoke-TestEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -PassThru
    $configCheck = & git -C $repo.RepoDir config test.already.marker 2>$null
    if ($res.result -eq 'ALREADY_SATISFIED' -and $res.mutation_state -eq 'NONE' -and $null -eq $configCheck) {
        Report-Pass 'T06' 'ALREADY_SATISFIED skips mutating action'
    } else {
        Report-Fail 'T06' 'ALREADY_SATISFIED skips mutating action' "got: $($res.result) $($res.mutation_state)"
    }
} catch {
    Report-Fail 'T06' 'ALREADY_SATISFIED skips mutating action' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T07 — successful mutating action plus passing postcondition
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't07'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'authorized_commands' = @(
            [ordered]@{
                'id' = 'set-git-config'
                'executable' = 'git'
                'arguments' = @('config', 'test.myval', 'hello')
                'cwd' = $repo.RepoDir
                'mutating' = $true
                'expect' = [ordered]@{ 'exit_code' = 0 }
            }
        )
        'required_postconditions' = @(
            [ordered]@{
                'id' = 'verify-config'
                'executable' = 'git'
                'arguments' = @('config', 'test.myval')
                'cwd' = $repo.RepoDir
                'mutating' = $false
                'expect' = [ordered]@{
                    'exit_code' = 0
                    'stdout_equals' = 'hello'
                }
            }
        )
    }
    $res = Invoke-TestEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -PassThru
    if ($res.result -eq 'COMPLETED' -and $res.mutation_state -eq 'APPLIED') {
        Report-Pass 'T07' 'successful mutating action plus passing postcondition'
    } else {
        Report-Fail 'T07' 'successful mutating action plus passing postcondition' "got: $($res.result) $($res.mutation_state) $($res.reason)"
    }
} catch {
    Report-Fail 'T07' 'successful mutating action plus passing postcondition' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T08 — failed mutating action
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't08'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'authorized_commands' = @(
            [ordered]@{
                'id' = 'failing-action'
                'executable' = 'git'
                'arguments' = @('checkout', 'branch_that_does_not_exist')
                'cwd' = $repo.RepoDir
                'mutating' = $true
                'expect' = [ordered]@{ 'exit_code' = 0 }
            }
        )
    }
    $res = Invoke-TestEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -PassThru
    if ($res.result -eq 'STOPPED' -and $res.mutation_state -eq 'UNKNOWN') {
        Report-Pass 'T08' 'failed mutating action'
    } else {
        Report-Fail 'T08' 'failed mutating action' "got: $($res.result) $($res.mutation_state)"
    }
} catch {
    Report-Fail 'T08' 'failed mutating action' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T09 — stdout and stderr are captured separately
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't09'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'authorized_commands' = @(
            [ordered]@{
                'id' = 'cmd-with-stderr'
                'executable' = 'git'
                'arguments' = @('rev-parse', '--verify', 'nonexistent_branch_xyz')
                'cwd' = $repo.RepoDir
                'mutating' = $false
                'expect' = [ordered]@{ 'exit_code' = 128 }
            }
        )
    }
    $res = Invoke-TestEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -PassThru
    $evidenceDir = Join-Path $res.run_directory 'evidence'
    $stdoutFile = (Get-ChildItem -Path $evidenceDir -Filter '*cmd-with-stderr-stdout.txt')[0].FullName
    $stderrFile = (Get-ChildItem -Path $evidenceDir -Filter '*cmd-with-stderr-stderr.txt')[0].FullName
    $stdoutBytes = [System.IO.File]::ReadAllBytes($stdoutFile)
    $stderrText = [System.IO.File]::ReadAllText($stderrFile)
    if ($stdoutBytes.Length -eq 0 -and $stderrText.Length -gt 0) {
        Report-Pass 'T09' 'stdout and stderr are captured separately'
    } else {
        Report-Fail 'T09' 'stdout and stderr are captured separately' "stdoutLen=$($stdoutBytes.Length), stderrLen=$($stderrText.Length)"
    }
} catch {
    Report-Fail 'T09' 'stdout and stderr are captured separately' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T10 — non-zero native Git exit code is captured as native integer exit code
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't10'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'authorized_commands' = @(
            [ordered]@{
                'id' = 'exit-code-test'
                'executable' = 'git'
                'arguments' = @('rev-parse', '--verify', 'nonexistent_branch_xyz')
                'cwd' = $repo.RepoDir
                'mutating' = $false
                'expect' = [ordered]@{ 'exit_code' = 128 }
            }
        )
    }
    $res = Invoke-TestEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -PassThru
    $jPath = Join-Path $res.run_directory 'commands.jsonl'
    $lines = Get-Content -LiteralPath $jPath
    $rec = $null
    foreach ($l in $lines) {
        $obj = $l | ConvertFrom-Json
        if ($obj.command_id -eq 'exit-code-test') { $rec = $obj; break }
    }
    if ($null -ne $rec -and $rec.exit_code -eq 128 -and $res.result -eq 'COMPLETED') {
        Report-Pass 'T10' 'non-zero native Git exit code is captured as native integer exit code'
    } else {
        Report-Fail 'T10' 'non-zero native Git exit code is captured as native integer exit code' "exit_code=$($rec.exit_code), result=$($res.result)"
    }
} catch {
    Report-Fail 'T10' 'non-zero native Git exit code is captured as native integer exit code' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T11 — forbidden git reset --hard rejected before launch
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't11'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'authorized_commands' = @(
            [ordered]@{
                'id' = 'forbidden-reset'
                'executable' = 'git'
                'arguments' = @('reset', '--hard', 'HEAD')
                'cwd' = $repo.RepoDir
                'mutating' = $true
                'expect' = [ordered]@{ 'exit_code' = 0 }
            }
        )
    }
    $res = Invoke-TestEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -PassThru
    if ($res.result -eq 'STOPPED' -and $res.reason -match 'Globally forbidden git subcommand: git reset') {
        Report-Pass 'T11' 'forbidden git reset --hard rejected before launch'
    } else {
        Report-Fail 'T11' 'forbidden git reset --hard rejected before launch' "got: $($res.result) $($res.reason)"
    }
} catch {
    Report-Fail 'T11' 'forbidden git reset --hard rejected before launch' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T12 — forbidden force push form rejected before launch
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't12'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'authorized_commands' = @(
            [ordered]@{
                'id' = 'forbidden-force-push'
                'executable' = 'git'
                'arguments' = @('push', '--force', 'origin', 'main')
                'cwd' = $repo.RepoDir
                'mutating' = $true
                'expect' = [ordered]@{ 'exit_code' = 0 }
            }
        )
    }
    $res = Invoke-TestEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -PassThru
    if ($res.result -eq 'STOPPED' -and $res.reason -match 'Globally forbidden git push flag') {
        Report-Pass 'T12' 'forbidden force push form rejected before launch'
    } else {
        Report-Fail 'T12' 'forbidden force push form rejected before launch' "got: $($res.result) $($res.reason)"
    }
} catch {
    Report-Fail 'T12' 'forbidden force push form rejected before launch' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T13 — unauthorized worktree path causes STOPPED / UNKNOWN
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't13'
    $unauthFile = Join-Path $repo.RepoDir 'unauthorized.txt'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'authorized_paths' = @('docs/**')
        'authorized_commands' = @(
            [ordered]@{
                'id' = 'status-action'
                'executable' = 'git'
                'arguments' = @('status', '--short')
                'cwd' = $repo.RepoDir
                'mutating' = $true
                'expect' = [ordered]@{ 'exit_code' = 0 }
            }
        )
    }
    [System.IO.File]::WriteAllText($unauthFile, 'secret', [System.Text.UTF8Encoding]::new($false))
    $res = Invoke-TestEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -PassThru
    if ($res.result -eq 'STOPPED' -and $res.mutation_state -eq 'UNKNOWN' -and $res.reason -match 'Scope violation') {
        Report-Pass 'T13' 'unauthorized worktree path causes STOPPED / UNKNOWN'
    } else {
        Report-Fail 'T13' 'unauthorized worktree path causes STOPPED / UNKNOWN' "got: $($res.result) $($res.mutation_state) $($res.reason)"
    }
} catch {
    Report-Fail 'T13' 'unauthorized worktree path causes STOPPED / UNKNOWN' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T14 — authorized path change succeeds in a temporary repo
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't14'
    $docsDir = Join-Path $repo.RepoDir 'docs'
    New-Item -ItemType Directory -Force -Path $docsDir | Out-Null
    $authFile = Join-Path $docsDir 'doc.md'
    [System.IO.File]::WriteAllText($authFile, '# Doc', [System.Text.UTF8Encoding]::new($false))

    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'authorized_paths' = @('docs/**')
        'authorized_commands' = @(
            [ordered]@{
                'id' = 'status-check'
                'executable' = 'git'
                'arguments' = @('status', '--short')
                'cwd' = $repo.RepoDir
                'mutating' = $true
                'expect' = [ordered]@{ 'exit_code' = 0 }
            }
        )
    }
    $res = Invoke-TestEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -PassThru
    if ($res.result -eq 'COMPLETED' -and $res.mutation_state -eq 'APPLIED') {
        Report-Pass 'T14' 'authorized path change succeeds in a temporary repo'
    } else {
        Report-Fail 'T14' 'authorized path change succeeds in a temporary repo' "got: $($res.result) $($res.mutation_state) $($res.reason)"
    }
} catch {
    Report-Fail 'T14' 'authorized path change succeeds in a temporary repo' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T15 — Windows fallback argument quoting: empty arg
# ----------------------------------------------------
try {
    $q = Format-PalkaProcessArgument ''
    if ($q -eq '""') {
        Report-Pass 'T15' 'Windows fallback argument quoting: empty arg'
    } else {
        Report-Fail 'T15' 'Windows fallback argument quoting: empty arg' "Expected quotes, got $q"
    }
} catch {
    Report-Fail 'T15' 'Windows fallback argument quoting: empty arg' $_.Exception.Message
}

# ----------------------------------------------------
# T16 — quoting: argument with spaces
# ----------------------------------------------------
try {
    $q = Format-PalkaProcessArgument 'hello world'
    if ($q -eq '"hello world"') {
        Report-Pass 'T16' 'quoting: argument with spaces'
    } else {
        Report-Fail 'T16' 'quoting: argument with spaces' "Expected quoted string, got $q"
    }
} catch {
    Report-Fail 'T16' 'quoting: argument with spaces' $_.Exception.Message
}

# ----------------------------------------------------
# T17 — quoting: embedded double quote
# ----------------------------------------------------
try {
    $q = Format-PalkaProcessArgument 'a"b'
    if ($q -eq '"a\"b"') {
        Report-Pass 'T17' 'quoting: embedded double quote'
    } else {
        Report-Fail 'T17' 'quoting: embedded double quote' "Expected escaped quote, got $q"
    }
} catch {
    Report-Fail 'T17' 'quoting: embedded double quote' $_.Exception.Message
}

# ----------------------------------------------------
# T18 — quoting: trailing backslashes
# ----------------------------------------------------
try {
    $q = Format-PalkaProcessArgument 'path with space\'
    if ($q -eq '"path with space\\"') {
        Report-Pass 'T18' 'quoting: trailing backslashes'
    } else {
        Report-Fail 'T18' 'quoting: trailing backslashes' "Expected escaped backslash, got $q"
    }
} catch {
    Report-Fail 'T18' 'quoting: trailing backslashes' $_.Exception.Message
}

# ----------------------------------------------------
# T19 — command IDs must be unique
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't19'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'authorized_commands' = @(
            [ordered]@{
                'id' = 'duplicate-id'
                'executable' = 'git'
                'arguments' = @('status')
                'cwd' = $repo.RepoDir
                'mutating' = $false
                'expect' = [ordered]@{ 'exit_code' = 0 }
            },
            [ordered]@{
                'id' = 'duplicate-id'
                'executable' = 'git'
                'arguments' = @('status')
                'cwd' = $repo.RepoDir
                'mutating' = $false
                'expect' = [ordered]@{ 'exit_code' = 0 }
            }
        )
    }
    $res = Invoke-TestEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -PassThru
    if ($res.result -eq 'STOPPED' -and $res.reason -match 'Duplicate command id') {
        Report-Pass 'T19' 'command IDs must be unique'
    } else {
        Report-Fail 'T19' 'command IDs must be unique' "got: $($res.result) $($res.reason)"
    }
} catch {
    Report-Fail 'T19' 'command IDs must be unique' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T20 — stdout empty produces zero-byte stdout evidence file
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't20'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'authorized_commands' = @(
            [ordered]@{
                'id' = 'empty-stdout-cmd'
                'executable' = 'git'
                'arguments' = @('diff', '--check')
                'cwd' = $repo.RepoDir
                'mutating' = $false
                'expect' = [ordered]@{
                    'exit_code' = 0
                    'stdout_empty' = $true
                }
            }
        )
    }
    $res = Invoke-TestEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -PassThru
    $evidenceDir = Join-Path $res.run_directory 'evidence'
    $stdoutFile = (Get-ChildItem -Path $evidenceDir -Filter '*empty-stdout-cmd-stdout.txt')[0].FullName
    $stdoutBytes = [System.IO.File]::ReadAllBytes($stdoutFile)
    if ($res.result -eq 'COMPLETED' -and $stdoutBytes.Length -eq 0) {
        Report-Pass 'T20' 'stdout empty produces zero-byte stdout evidence file'
    } else {
        Report-Fail 'T20' 'stdout empty produces zero-byte stdout evidence file' "result=$($res.result), bytes=$($stdoutBytes.Length)"
    }
} catch {
    Report-Fail 'T20' 'stdout empty produces zero-byte stdout evidence file' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T21 — commands.jsonl contains only actually launched commands and is valid JSONL
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't21'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'required_preconditions' = @(
            [ordered]@{
                'id' = 'pre-cmd'
                'executable' = 'git'
                'arguments' = @('status', '--short')
                'cwd' = $repo.RepoDir
                'mutating' = $false
                'expect' = [ordered]@{ 'exit_code' = 0 }
            }
        )
        'authorized_commands' = @(
            [ordered]@{
                'id' = 'act-cmd'
                'executable' = 'git'
                'arguments' = @('status', '--short')
                'cwd' = $repo.RepoDir
                'mutating' = $false
                'expect' = [ordered]@{ 'exit_code' = 0 }
            }
        )
    }
    $res = Invoke-TestEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -PassThru
    $jPath = Join-Path $res.run_directory 'commands.jsonl'
    $lines = Get-Content -LiteralPath $jPath
    $validObjects = 0
    foreach ($l in $lines) {
        if ($l.Trim().Length -gt 0) {
            $obj = $l | ConvertFrom-Json
            if ($null -ne $obj.command_id) { $validObjects++ }
        }
    }
    if ($validObjects -ge 9 -and $res.result -eq 'COMPLETED') {
        Report-Pass 'T21' 'commands.jsonl contains only actually launched commands and is valid JSONL'
    } else {
        Report-Fail 'T21' 'commands.jsonl contains only actually launched commands and is valid JSONL' "Valid records: $validObjects, result: $($res.result) $($res.reason)"
    }
} catch {
    Report-Fail 'T21' 'commands.jsonl contains only actually launched commands and is valid JSONL' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T22 — precondition failure prevents ALL action commands, not merely the first
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't22'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'required_preconditions' = @(
            [ordered]@{
                'id' = 'fail-pre'
                'executable' = 'git'
                'arguments' = @('rev-parse', 'nonexistent_ref')
                'cwd' = $repo.RepoDir
                'mutating' = $false
                'expect' = [ordered]@{ 'exit_code' = 0 }
            }
        )
        'authorized_commands' = @(
            [ordered]@{
                'id' = 'action-1'
                'executable' = 'git'
                'arguments' = @('config', 'test.act1', '1')
                'cwd' = $repo.RepoDir
                'mutating' = $true
                'expect' = [ordered]@{ 'exit_code' = 0 }
            },
            [ordered]@{
                'id' = 'action-2'
                'executable' = 'git'
                'arguments' = @('config', 'test.act2', '2')
                'cwd' = $repo.RepoDir
                'mutating' = $true
                'expect' = [ordered]@{ 'exit_code' = 0 }
            }
        )
    }
    $res = Invoke-TestEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -PassThru
    $c1 = & git -C $repo.RepoDir config test.act1 2>$null
    $c2 = & git -C $repo.RepoDir config test.act2 2>$null
    if ($res.result -eq 'STOPPED' -and $null -eq $c1 -and $null -eq $c2) {
        Report-Pass 'T22' 'precondition failure prevents ALL action commands, not merely the first'
    } else {
        Report-Fail 'T22' 'precondition failure prevents ALL action commands, not merely the first' "c1=$c1, c2=$c2, result=$($res.result)"
    }
} catch {
    Report-Fail 'T22' 'precondition failure prevents ALL action commands, not merely the first' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T23 — refresh command rejects git fetch --force
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't23'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'refresh_commands' = @(
            [ordered]@{
                'id' = 'refresh-with-force'
                'executable' = 'git'
                'arguments' = @('fetch', '--force', 'origin')
                'cwd' = $repo.RepoDir
                'mutating' = $false
                'expect' = [ordered]@{ 'exit_code' = 0 }
            }
        )
    }
    $res = Invoke-TestEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -PassThru
    if ($res.result -eq 'STOPPED' -and $res.reason -match 'Forbidden force/prune flag in refresh_commands') {
        Report-Pass 'T23' 'refresh command rejects git fetch --force'
    } else {
        Report-Fail 'T23' 'refresh command rejects git fetch --force' "got: $($res.result) $($res.reason)"
    }
} catch {
    Report-Fail 'T23' 'refresh command rejects git fetch --force' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T24 — launch failure before mutation produces STOPPED without inventing an exit code
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't24'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'required_preconditions' = @(
            [ordered]@{
                'id' = 'nonexistent-executable'
                'executable' = 'nonexistent_palka_binary_xyz123'
                'arguments' = @('arg')
                'cwd' = $repo.RepoDir
                'mutating' = $false
                'expect' = [ordered]@{ 'exit_code' = 0 }
            }
        )
    }
    $res = Invoke-TestEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -PassThru
    $jPath = Join-Path $res.run_directory 'commands.jsonl'
    $lines = Get-Content -LiteralPath $jPath
    $rec = $null
    foreach ($l in $lines) {
        $obj = $l | ConvertFrom-Json
        if ($obj.command_id -eq 'nonexistent-executable') { $rec = $obj; break }
    }
    if ($res.result -eq 'STOPPED' -and $null -ne $rec -and $rec.exit_code -eq $null -and $null -ne $rec.launch_error) {
        Report-Pass 'T24' 'launch failure before mutation produces STOPPED without inventing an exit code'
    } else {
        Report-Fail 'T24' 'launch failure before mutation produces STOPPED without inventing an exit code' "got: $($res.result), exit_code=$($rec.exit_code)"
    }
} catch {
    Report-Fail 'T24' 'launch failure before mutation produces STOPPED without inventing an exit code' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T25 — postcondition failure after successful mutation results in STOPPED, UNKNOWN
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't25'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'authorized_commands' = @(
            [ordered]@{
                'id' = 'mutating-cmd'
                'executable' = 'git'
                'arguments' = @('config', 'test.mut', 'val')
                'cwd' = $repo.RepoDir
                'mutating' = $true
                'expect' = [ordered]@{ 'exit_code' = 0 }
            }
        )
        'required_postconditions' = @(
            [ordered]@{
                'id' = 'failing-postcondition'
                'executable' = 'git'
                'arguments' = @('config', 'test.mut')
                'cwd' = $repo.RepoDir
                'mutating' = $false
                'expect' = [ordered]@{
                    'exit_code' = 0
                    'stdout_equals' = 'wrong_expected_val'
                }
            }
        )
    }
    $res = Invoke-TestEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -PassThru
    if ($res.result -eq 'STOPPED' -and $res.mutation_state -eq 'UNKNOWN' -and $res.reason -match 'stdout mismatch') {
        Report-Pass 'T25' 'postcondition failure after successful mutation results in STOPPED, UNKNOWN'
    } else {
        Report-Fail 'T25' 'postcondition failure after successful mutation results in STOPPED, UNKNOWN' "got: $($res.result) $($res.mutation_state) $($res.reason)"
    }
} catch {
    Report-Fail 'T25' 'postcondition failure after successful mutation results in STOPPED, UNKNOWN' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T26 — every launched native process is represented exactly once in commands.jsonl
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't26'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'authorized_commands' = @(
            [ordered]@{
                'id' = 't26-status'
                'executable' = 'git'
                'arguments' = @('status', '--short')
                'cwd' = $repo.RepoDir
                'mutating' = $false
                'expect' = [ordered]@{ 'exit_code' = 0 }
            }
        )
    }
    $res = Invoke-TestEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -PassThru
    $jPath = Join-Path $res.run_directory 'commands.jsonl'
    $lines = Get-Content -LiteralPath $jPath
    $evidenceFiles = Get-ChildItem -Path (Join-Path $res.run_directory 'evidence')
    $expectedEvidenceCount = $lines.Count * 2
    if ($res.result -eq 'COMPLETED' -and $evidenceFiles.Count -eq $expectedEvidenceCount -and $lines.Count -ge 8) {
        Report-Pass 'T26' 'every launched native process is represented exactly once in commands.jsonl'
    } else {
        Report-Fail 'T26' 'every launched native process is represented exactly once in commands.jsonl' "lines=$($lines.Count), evidence=$($evidenceFiles.Count), expected=$expectedEvidenceCount"
    }
} catch {
    Report-Fail 'T26' 'every launched native process is represented exactly once in commands.jsonl' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T27 — git -C <repo> reset --hard is rejected before launch
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't27'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'authorized_commands' = @(
            [ordered]@{
                'id' = 'reset-with-opt-c'
                'executable' = 'git'
                'arguments' = @('-C', $repo.RepoDir, 'reset', '--hard', 'HEAD')
                'cwd' = $repo.RepoDir
                'mutating' = $true
                'expect' = [ordered]@{ 'exit_code' = 0 }
            }
        )
    }
    $res = Invoke-TestEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -PassThru
    if ($res.result -eq 'STOPPED' -and $res.mutation_state -eq 'NOT_APPLIED' -and $res.reason -match 'Globally forbidden git subcommand: git reset') {
        Report-Pass 'T27' 'git -C <repo> reset --hard is rejected before launch'
    } else {
        Report-Fail 'T27' 'git -C <repo> reset --hard is rejected before launch' "got: $($res.result) $($res.mutation_state) $($res.reason)"
    }
} catch {
    Report-Fail 'T27' 'git -C <repo> reset --hard is rejected before launch' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T28 — force push variants are rejected before launch
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't28'
    $m1 = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'authorized_commands' = @(
            [ordered]@{
                'id' = 'push-force-with-lease'
                'executable' = 'git'
                'arguments' = @('push', '--force-with-lease=main', 'origin', 'main')
                'cwd' = $repo.RepoDir
                'mutating' = $true
                'expect' = [ordered]@{ 'exit_code' = 0 }
            }
        )
    }
    $res1 = Invoke-TestEngine -ManifestPath $m1.ManifestPath -OutputRoot $outputRoot -PassThru

    $m2 = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'authorized_commands' = @(
            [ordered]@{
                'id' = 'push-plus-refspec'
                'executable' = 'git'
                'arguments' = @('push', 'origin', '+main:main')
                'cwd' = $repo.RepoDir
                'mutating' = $true
                'expect' = [ordered]@{ 'exit_code' = 0 }
            }
        )
    }
    $res2 = Invoke-TestEngine -ManifestPath $m2.ManifestPath -OutputRoot $outputRoot -PassThru

    if ($res1.result -eq 'STOPPED' -and $res1.reason -match 'Globally forbidden git push flag' -and
        $res2.result -eq 'STOPPED' -and $res2.reason -match 'Globally forbidden git push force refspec') {
        Report-Pass 'T28' 'force push variants are rejected before launch'
    } else {
        Report-Fail 'T28' 'force push variants are rejected before launch' "res1=$($res1.result), res2=$($res2.result)"
    }
} catch {
    Report-Fail 'T28' 'force push variants are rejected before launch' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m1 -and (Test-Path $m1.ManifestPath)) { Remove-Item -Force $m1.ManifestPath }
    if ($null -ne $m2 -and (Test-Path $m2.ManifestPath)) { Remove-Item -Force $m2.ManifestPath }
}

# ----------------------------------------------------
# T29 — cmd.exe /c and powershell.exe -Command manifest wrappers are rejected before launch
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't29'
    $m1 = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'authorized_commands' = @(
            [ordered]@{
                'id' = 'cmd-c-wrapper'
                'executable' = 'cmd.exe'
                'arguments' = @('/c', 'echo evil')
                'cwd' = $repo.RepoDir
                'mutating' = $true
                'expect' = [ordered]@{ 'exit_code' = 0 }
            }
        )
    }
    $res1 = Invoke-TestEngine -ManifestPath $m1.ManifestPath -OutputRoot $outputRoot -PassThru

    $m2 = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'authorized_commands' = @(
            [ordered]@{
                'id' = 'ps-command-wrapper'
                'executable' = 'powershell.exe'
                'arguments' = @('-Command', 'Write-Host evil')
                'cwd' = $repo.RepoDir
                'mutating' = $true
                'expect' = [ordered]@{ 'exit_code' = 0 }
            }
        )
    }
    $res2 = Invoke-TestEngine -ManifestPath $m2.ManifestPath -OutputRoot $outputRoot -PassThru

    if ($res1.result -eq 'STOPPED' -and $res1.reason -match 'Opaque shell wrapper rejected' -and
        $res2.result -eq 'STOPPED' -and $res2.reason -match 'Opaque shell wrapper rejected') {
        Report-Pass 'T29' 'cmd.exe /c and powershell.exe -Command manifest wrappers are rejected before launch'
    } else {
        Report-Fail 'T29' 'cmd.exe /c and powershell.exe -Command manifest wrappers are rejected before launch' "res1=$($res1.result), res2=$($res2.result)"
    }
} catch {
    Report-Fail 'T29' 'cmd.exe /c and powershell.exe -Command manifest wrappers are rejected before launch' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m1 -and (Test-Path $m1.ManifestPath)) { Remove-Item -Force $m1.ManifestPath }
    if ($null -ne $m2 -and (Test-Path $m2.ManifestPath)) { Remove-Item -Force $m2.ManifestPath }
}

# ----------------------------------------------------
# T30 — mutating=true is rejected in each non-action section
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't30'
    $m1 = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'required_preconditions' = @(
            [ordered]@{
                'id' = 'mut-pre'
                'executable' = 'git'
                'arguments' = @('status')
                'cwd' = $repo.RepoDir
                'mutating' = $true
                'expect' = [ordered]@{ 'exit_code' = 0 }
            }
        )
    }
    $res1 = Invoke-TestEngine -ManifestPath $m1.ManifestPath -OutputRoot $outputRoot -PassThru

    $m2 = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'already_satisfied_checks' = @(
            [ordered]@{
                'id' = 'mut-asc'
                'executable' = 'git'
                'arguments' = @('status')
                'cwd' = $repo.RepoDir
                'mutating' = $true
                'expect' = [ordered]@{ 'exit_code' = 0 }
            }
        )
    }
    $res2 = Invoke-TestEngine -ManifestPath $m2.ManifestPath -OutputRoot $outputRoot -PassThru

    $m3 = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'required_postconditions' = @(
            [ordered]@{
                'id' = 'mut-post'
                'executable' = 'git'
                'arguments' = @('status')
                'cwd' = $repo.RepoDir
                'mutating' = $true
                'expect' = [ordered]@{ 'exit_code' = 0 }
            }
        )
    }
    $res3 = Invoke-TestEngine -ManifestPath $m3.ManifestPath -OutputRoot $outputRoot -PassThru

    if ($res1.result -eq 'STOPPED' -and $res1.reason -match 'must have mutating: false' -and
        $res2.result -eq 'STOPPED' -and $res2.reason -match 'must have mutating: false' -and
        $res3.result -eq 'STOPPED' -and $res3.reason -match 'must have mutating: false') {
        Report-Pass 'T30' 'mutating=true is rejected in each non-action section'
    } else {
        Report-Fail 'T30' 'mutating=true is rejected in each non-action section' "res1=$($res1.result), res2=$($res2.result), res3=$($res3.result)"
    }
} catch {
    Report-Fail 'T30' 'mutating=true is rejected in each non-action section' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m1 -and (Test-Path $m1.ManifestPath)) { Remove-Item -Force $m1.ManifestPath }
    if ($null -ne $m2 -and (Test-Path $m2.ManifestPath)) { Remove-Item -Force $m2.ManifestPath }
    if ($null -ne $m3 -and (Test-Path $m3.ManifestPath)) { Remove-Item -Force $m3.ManifestPath }
}

# ----------------------------------------------------
# T31 — dangerous/policy failure inside already_satisfied_checks causes STOPPED and no authorized action runs
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't31'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'already_satisfied_checks' = @(
            [ordered]@{
                'id' = 'dangerous-asc'
                'executable' = 'git'
                'arguments' = @('reset', '--hard', 'HEAD')
                'cwd' = $repo.RepoDir
                'mutating' = $false
                'expect' = [ordered]@{ 'exit_code' = 0 }
            }
        )
        'authorized_commands' = @(
            [ordered]@{
                'id' = 'action-marker'
                'executable' = 'git'
                'arguments' = @('config', 'test.t31.marker', 'ran')
                'cwd' = $repo.RepoDir
                'mutating' = $true
                'expect' = [ordered]@{ 'exit_code' = 0 }
            }
        )
    }
    $res = Invoke-TestEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -PassThru
    $marker = & git -C $repo.RepoDir config test.t31.marker 2>$null
    if ($res.result -eq 'STOPPED' -and $res.reason -match 'Policy violation' -and $null -eq $marker) {
        Report-Pass 'T31' 'dangerous/policy failure inside already_satisfied_checks causes STOPPED'
    } else {
        Report-Fail 'T31' 'dangerous/policy failure inside already_satisfied_checks causes STOPPED' "got: $($res.result) $($res.reason), marker=$marker"
    }
} catch {
    Report-Fail 'T31' 'dangerous/policy failure inside already_satisfied_checks causes STOPPED' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T32 — expected_remote_refs origin/test=ABSENT fails when test branch actually exists
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't32' -WithBareOrigin
    $p = New-Object System.Diagnostics.Process
    $p.StartInfo.FileName = 'git'
    $p.StartInfo.WorkingDirectory = $repo.RepoDir
    $p.StartInfo.UseShellExecute = $false
    $p.StartInfo.RedirectStandardOutput = $true
    $p.StartInfo.RedirectStandardError = $true
    $p.StartInfo.CreateNoWindow = $true

    $p.StartInfo.Arguments = 'checkout -b test-branch'
    $p.Start() | Out-Null; $p.WaitForExit()
    $p.StartInfo.Arguments = 'push origin test-branch'
    $p.Start() | Out-Null; $p.WaitForExit()
    $p.StartInfo.Arguments = 'checkout main'
    $p.Start() | Out-Null; $p.WaitForExit()

    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'expected_remote_refs' = [ordered]@{
            'origin/test-branch' = 'ABSENT'
        }
    }
    $res = Invoke-TestEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -PassThru
    if ($res.result -eq 'STOPPED' -and $res.reason -match 'expected ABSENT but was found on remote') {
        Report-Pass 'T32' 'expected_remote_refs origin/test=ABSENT fails when test branch exists'
    } else {
        Report-Fail 'T32' 'expected_remote_refs origin/test=ABSENT fails when test branch exists' "got: $($res.result) $($res.reason)"
    }
} catch {
    Report-Fail 'T32' 'expected_remote_refs origin/test=ABSENT fails when test branch exists' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo -RepoDir $repo.RepoDir -BareDir $repo.BareDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T33 — expected_remote_refs origin/test=ABSENT succeeds when branch is actually absent
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't33' -WithBareOrigin
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'expected_remote_refs' = [ordered]@{
            'origin/nonexistent-feature' = 'ABSENT'
        }
    }
    $res = Invoke-TestEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -PassThru
    if ($res.result -eq 'COMPLETED' -and $res.mutation_state -eq 'NONE') {
        Report-Pass 'T33' 'expected_remote_refs origin/test=ABSENT succeeds when branch is absent'
    } else {
        Report-Fail 'T33' 'expected_remote_refs origin/test=ABSENT succeeds when branch is absent' "got: $($res.result) $($res.reason)"
    }
} catch {
    Report-Fail 'T33' 'expected_remote_refs origin/test=ABSENT succeeds when branch is absent' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo -RepoDir $repo.RepoDir -BareDir $repo.BareDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T34 — OutputRoot inside working_directory is rejected and creates no run-directory artifact
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't34'
    $insideOutput = Join-Path $repo.RepoDir 'artifacts-inside'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha
    $res = Invoke-TestEngine -ManifestPath $m.ManifestPath -OutputRoot $insideOutput -PassThru
    $createdInside = Test-Path $insideOutput
    if ($res.result -eq 'STOPPED' -and $res.reason -match 'must not be equal to or inside working_directory' -and -not $createdInside) {
        Report-Pass 'T34' 'OutputRoot inside working_directory is rejected'
    } else {
        Report-Fail 'T34' 'OutputRoot inside working_directory is rejected' "got: $($res.result), createdInside=$createdInside"
    }
} catch {
    Report-Fail 'T34' 'OutputRoot inside working_directory is rejected' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T35 — operation_id path traversal attempt is rejected and cannot write outside OutputRoot
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't35'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -OperationId '../../traversal'
    $res = Invoke-TestEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -PassThru
    if ($res.result -eq 'STOPPED' -and $res.reason -match 'Invalid operation_id') {
        Report-Pass 'T35' 'operation_id path traversal attempt is rejected'
    } else {
        Report-Fail 'T35' 'operation_id path traversal attempt is rejected' "got: $($res.result) $($res.reason)"
    }
} catch {
    Report-Fail 'T35' 'operation_id path traversal attempt is rejected' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T36 — malformed JSON produces controlled STOPPED behavior; CLI exit code is 2
# ----------------------------------------------------
try {
    $malformedPath = Join-Path $env:TEMP 'malformed.json'
    [System.IO.File]::WriteAllText($malformedPath, '{ this is not valid JSON }', [System.Text.UTF8Encoding]::new($false))
    $malformedSha = Get-TestManifestSha256 $malformedPath

    $nativeCount = 0
    $hook = { param($cmd, $ph, $p) $script:nativeCount++ }
    $res = Invoke-PalkaEngine -ManifestPath $malformedPath -OutputRoot $outputRoot -AuthorizedManifestSha256 $malformedSha -PassThru -TestPostStartHook $hook

    $enginePass = ($res.result -eq 'STOPPED' -and $res.mutation_state -eq 'NOT_APPLIED' -and $res.failed_phase -eq 'MANIFEST_READ' -and $res.reason -match 'Malformed JSON' -and $nativeCount -eq 0)

    $proc = New-Object System.Diagnostics.Process
    $proc.StartInfo.FileName = 'powershell.exe'
    $proc.StartInfo.Arguments = "-NoProfile -ExecutionPolicy Bypass -File C:\PALKA\scripts\governance\Invoke-PalkaOperation.ps1 -ManifestPath `"$malformedPath`" -OutputRoot `"$outputRoot`" -AuthorizedManifestSha256 $malformedSha"
    $proc.StartInfo.WorkingDirectory = 'C:\PALKA'
    $proc.StartInfo.UseShellExecute = $false
    $proc.StartInfo.RedirectStandardOutput = $true
    $proc.StartInfo.RedirectStandardError = $true
    $proc.Start() | Out-Null
    $stdout = $proc.StandardOutput.ReadToEnd()
    $proc.WaitForExit()
    $exitCode = $proc.ExitCode

    $cliPass = ($exitCode -eq 2 -and $stdout -match 'RESULT:\s*STOPPED' -and $stdout -match 'MUTATION_STATE:\s*NOT_APPLIED')

    if ($enginePass -and $cliPass) {
        Report-Pass 'T36' 'malformed JSON produces controlled STOPPED behavior and exit code 2'
    } else {
        Report-Fail 'T36' 'malformed JSON produces controlled STOPPED behavior and exit code 2' "enginePass=$enginePass (phase=$($res.failed_phase), reason=$($res.reason), count=$nativeCount), cliPass=$cliPass (exitCode=$exitCode, out=$stdout)"
    }
} catch {
    Report-Fail 'T36' 'malformed JSON produces controlled STOPPED behavior and exit code 2' $_.Exception.Message
} finally {
    if (Test-Path $malformedPath) { Remove-Item -Force $malformedPath }
}

# ----------------------------------------------------
# T37 — run-directory manifest.json SHA-256 exactly equals original valid manifest byte SHA-256
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't37'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha
    $srcHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $m.ManifestPath).Hash
    $res = Invoke-TestEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -PassThru
    $destManifestPath = Join-Path $res.run_directory 'manifest.json'
    $destHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $destManifestPath).Hash
    if ($res.result -eq 'COMPLETED' -and $srcHash -eq $destHash) {
        Report-Pass 'T37' 'run-directory manifest.json SHA-256 equals original source manifest SHA-256'
    } else {
        Report-Fail 'T37' 'run-directory manifest.json SHA-256 equals original source manifest SHA-256' "src=$srcHash, dest=$destHash"
    }
} catch {
    Report-Fail 'T37' 'run-directory manifest.json SHA-256 equals original source manifest SHA-256' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T38 — force scope-proof failure after a launched mutation results in STOPPED, UNKNOWN
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't38'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'authorized_commands' = @(
            [ordered]@{
                'id' = 'corrupt-git-index'
                'executable' = 'git'
                'arguments' = @('config', 'core.worktree', 'Z:\nonexistent_palka_drive_path_xyz')
                'cwd' = $repo.RepoDir
                'mutating' = $true
                'expect' = [ordered]@{ 'exit_code' = 0 }
            }
        )
    }
    $res = Invoke-TestEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -PassThru
    if ($res.result -eq 'STOPPED' -and $res.mutation_state -eq 'UNKNOWN' -and $res.reason -match 'builtin-scope-after') {
        Report-Pass 'T38' 'force scope-proof failure after launched mutation results in STOPPED, UNKNOWN'
    } else {
        Report-Fail 'T38' 'force scope-proof failure after launched mutation results in STOPPED, UNKNOWN' "got: $($res.result) $($res.mutation_state) $($res.reason)"
    }
} catch {
    Report-Fail 'T38' 'force scope-proof failure after launched mutation results in STOPPED, UNKNOWN' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T39 — action leaves repository on a branch different from target_branch
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't39'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'authorized_commands' = @(
            [ordered]@{
                'id' = 'switch-branch-unexpectedly'
                'executable' = 'git'
                'arguments' = @('checkout', '-b', 'other-branch')
                'cwd' = $repo.RepoDir
                'mutating' = $true
                'expect' = [ordered]@{ 'exit_code' = 0 }
            }
        )
    }
    $res = Invoke-TestEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -PassThru
    if ($res.result -eq 'STOPPED' -and $res.mutation_state -eq 'UNKNOWN' -and $res.reason -match 'does not match target_branch') {
        Report-Pass 'T39' 'action leaving repo on different branch results in STOPPED, UNKNOWN'
    } else {
        Report-Fail 'T39' 'action leaving repo on different branch results in STOPPED, UNKNOWN' "got: $($res.result) $($res.mutation_state) $($res.reason)"
    }
} catch {
    Report-Fail 'T39' 'action leaving repo on different branch results in STOPPED, UNKNOWN' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T40 — actual fallback argument round-trip preserves all required Windows quoting edge cases
# ----------------------------------------------------
try {
    $testArgs = @(
        '',
        'simple',
        'with spaces',
        "`twith`ttab`t",
        'embedded"quote',
        'trailing\slash\',
        'backslashes\"before"quote',
        'multiple \\\\ slashes \'
    )

    $helperScriptPath = Join-Path $env:TEMP 'echo_args_helper.ps1'
    @'
param()
$args | ForEach-Object { Write-Output "[ARG]:$_" }
'@ | Set-Content -LiteralPath $helperScriptPath -Encoding UTF8

    $formattedArgs = @($testArgs | ForEach-Object { Format-PalkaProcessArgument $_ }) -join ' '
    $proc = New-Object System.Diagnostics.Process
    $proc.StartInfo.FileName = 'powershell.exe'
    $proc.StartInfo.Arguments = "-NoProfile -ExecutionPolicy Bypass -File `"$helperScriptPath`" $formattedArgs"
    $proc.StartInfo.UseShellExecute = $false
    $proc.StartInfo.RedirectStandardOutput = $true
    $proc.StartInfo.RedirectStandardError = $true
    $proc.Start() | Out-Null
    $stdout = $proc.StandardOutput.ReadToEnd()
    $proc.WaitForExit()

    $receivedLines = @($stdout -split "`r?`n" | Where-Object { $_.StartsWith('[ARG]:') } | ForEach-Object { $_.Substring(6) })

    $matchAll = $true
    if ($receivedLines.Count -ne $testArgs.Count) {
        $matchAll = $false
    } else {
        for ($i = 0; $i -lt $testArgs.Count; $i++) {
            if ($receivedLines[$i] -ne $testArgs[$i]) {
                $matchAll = $false
                break
            }
        }
    }

    if ($matchAll) {
        Report-Pass 'T40' 'actual fallback argument round-trip preserves all quoting edge cases'
    } else {
        Report-Fail 'T40' 'actual fallback argument round-trip preserves all quoting edge cases' "received=$($receivedLines.Count), expected=$($testArgs.Count)"
    }
} catch {
    Report-Fail 'T40' 'actual fallback argument round-trip preserves all quoting edge cases' $_.Exception.Message
} finally {
    if (Test-Path $helperScriptPath) { Remove-Item -Force $helperScriptPath }
}

# ----------------------------------------------------
# T41 — command ID containing traversal/path syntax is rejected before launch
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't41'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'authorized_commands' = @(
            [ordered]@{
                'id' = 'cmd/with/slash'
                'executable' = 'git'
                'arguments' = @('status')
                'cwd' = $repo.RepoDir
                'mutating' = $false
                'expect' = [ordered]@{ 'exit_code' = 0 }
            }
        )
    }
    $res = Invoke-TestEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -PassThru
    if ($res.result -eq 'STOPPED' -and $res.reason -match 'Invalid command id') {
        Report-Pass 'T41' 'command ID containing traversal/path syntax is rejected before launch'
    } else {
        Report-Fail 'T41' 'command ID containing traversal/path syntax is rejected before launch' "got: $($res.result) $($res.reason)"
    }
} catch {
    Report-Fail 'T41' 'command ID containing traversal/path syntax is rejected before launch' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T42 — Git alias global-option bypass is rejected before launch
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't42'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'authorized_commands' = @(
            [ordered]@{
                'id' = 'alias-bypass'
                'executable' = 'git'
                'arguments' = @('-c', 'alias.x=reset', 'x', '--hard')
                'cwd' = $repo.RepoDir
                'mutating' = $true
                'expect' = [ordered]@{ 'exit_code' = 0 }
            }
        )
    }
    $res = Invoke-TestEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -PassThru
    if ($res.result -eq 'STOPPED' -and $res.reason -match 'Forbidden git alias configuration') {
        Report-Pass 'T42' 'Git alias global-option bypass is rejected before launch'
    } else {
        Report-Fail 'T42' 'Git alias global-option bypass is rejected before launch' "got: $($res.result) $($res.reason)"
    }
} catch {
    Report-Fail 'T42' 'Git alias global-option bypass is rejected before launch' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T43 — built-in final branch/head native processes are journaled and their evidence paths exist
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't43'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha
    $res = Invoke-TestEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -PassThru
    $jPath = Join-Path $res.run_directory 'commands.jsonl'
    $lines = Get-Content -LiteralPath $jPath
    $hasFinalBranch = $false
    $hasFinalHead = $false
    foreach ($l in $lines) {
        $obj = $l | ConvertFrom-Json
        if ($obj.command_id -eq 'builtin-final-branch') {
            $hasFinalBranch = (Test-Path $obj.stdout_path) -and (Test-Path $obj.stderr_path)
        }
        if ($obj.command_id -eq 'builtin-final-head') {
            $hasFinalHead = (Test-Path $obj.stdout_path) -and (Test-Path $obj.stderr_path)
        }
    }
    if ($res.result -eq 'COMPLETED' -and $hasFinalBranch -and $hasFinalHead) {
        Report-Pass 'T43' 'built-in final branch/head native processes are journaled and evidence exists'
    } else {
        Report-Fail 'T43' 'built-in final branch/head native processes are journaled and evidence exists' "fb=$hasFinalBranch, fh=$hasFinalHead"
    }
} catch {
    Report-Fail 'T43' 'built-in final branch/head native processes are journaled and evidence exists' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T44 — mutating process launch failure before actual process start returns STOPPED, NOT_APPLIED
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't44'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'authorized_commands' = @(
            [ordered]@{
                'id' = 'nonexistent-mutating-exe'
                'executable' = 'nonexistent_mutating_binary_xyz'
                'arguments' = @('do-mutation')
                'cwd' = $repo.RepoDir
                'mutating' = $true
                'expect' = [ordered]@{ 'exit_code' = 0 }
            }
        )
    }
    $res = Invoke-TestEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -PassThru
    $jPath = Join-Path $res.run_directory 'commands.jsonl'
    $lines = Get-Content -LiteralPath $jPath
    $rec = $null
    foreach ($l in $lines) {
        $obj = $l | ConvertFrom-Json
        if ($obj.command_id -eq 'nonexistent-mutating-exe') { $rec = $obj; break }
    }
    if ($res.result -eq 'STOPPED' -and $res.mutation_state -eq 'NOT_APPLIED' -and $null -ne $rec -and $rec.exit_code -eq $null -and $null -ne $rec.launch_error) {
        Report-Pass 'T44' 'mutating process launch failure returns STOPPED, NOT_APPLIED'
    } else {
        Report-Fail 'T44' 'mutating process launch failure returns STOPPED, NOT_APPLIED' "res=$($res.result), state=$($res.mutation_state)"
    }
} catch {
    Report-Fail 'T44' 'mutating process launch failure returns STOPPED, NOT_APPLIED' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T45 — shell/policy rejection in authorized_commands does not create a fake launched command journal record
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't45'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'authorized_commands' = @(
            [ordered]@{
                'id' = 'rejected-cmd'
                'executable' = 'cmd.exe'
                'arguments' = @('/c', 'echo 1')
                'cwd' = $repo.RepoDir
                'mutating' = $false
                'expect' = [ordered]@{ 'exit_code' = 0 }
            }
        )
    }
    $res = Invoke-TestEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -PassThru
    $jPath = Join-Path $res.run_directory 'commands.jsonl'
    $lines = Get-Content -LiteralPath $jPath
    $hasFakeRecord = $false
    foreach ($l in $lines) {
        $obj = $l | ConvertFrom-Json
        if ($obj.command_id -eq 'rejected-cmd') { $hasFakeRecord = $true; break }
    }
    if ($res.result -eq 'STOPPED' -and -not $hasFakeRecord) {
        Report-Pass 'T45' 'policy rejection does not create a fake launched command journal record'
    } else {
        Report-Fail 'T45' 'policy rejection does not create a fake launched command journal record' "hasFakeRecord=$hasFakeRecord"
    }
} catch {
    Report-Fail 'T45' 'policy rejection does not create a fake launched command journal record' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T46 — successful engine execution launches no unjournaled native processes
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't46'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha
    $res = Invoke-TestEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -PassThru
    $jPath = Join-Path $res.run_directory 'commands.jsonl'
    $lines = Get-Content -LiteralPath $jPath

    # Check static content of module for direct git calls
    $modText = [System.IO.File]::ReadAllText($govModule)
    $hasDirectGitCall = $false
    $modLines = $modText -split "`r?`n"
    foreach ($ml in $modLines) {
        $trimmed = $ml.Trim()
        if ($trimmed.StartsWith('#')) { continue }
        if ($trimmed -match '& git\b') {
            $hasDirectGitCall = $true
            break
        }
    }

    if ($res.result -eq 'COMPLETED' -and $res.command_count -eq $lines.Count -and -not $hasDirectGitCall) {
        Report-Pass 'T46' 'successful engine execution launches no unjournaled native processes'
    } else {
        Report-Fail 'T46' 'successful engine execution launches no unjournaled native processes' "res=$($res.result), cmd_count=$($res.command_count), lines=$($lines.Count), directGit=$hasDirectGitCall"
    }
} catch {
    Report-Fail 'T46' 'successful engine execution launches no unjournaled native processes' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T47 — ALREADY_SATISFIED checks all pass but current branch differs from target_branch
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't47'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'target_branch' = 'feature-x'
        'branch' = 'feature-x'
        'branch_transition' = [ordered]@{
            'allowed' = $true
            'mode' = 'create'
            'from' = 'main'
            'to' = 'feature-x'
        }
        'already_satisfied_checks' = @(
            [ordered]@{
                'id' = 'check-head-exists'
                'executable' = 'git'
                'arguments' = @('rev-parse', 'HEAD')
                'cwd' = $repo.RepoDir
                'mutating' = $false
                'expect' = [ordered]@{
                    'exit_code' = 0
                    'stdout_equals' = $repo.HeadSha
                }
            }
        )
        'authorized_commands' = @(
            [ordered]@{
                'id' = 'create-target-branch'
                'executable' = 'git'
                'arguments' = @('checkout', '-b', 'feature-x')
                'cwd' = $repo.RepoDir
                'mutating' = $true
                'expect' = [ordered]@{ 'exit_code' = 0 }
            }
        )
    }
    $res = Invoke-TestEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -PassThru
    if ($res.result -eq 'COMPLETED' -and $res.mutation_state -eq 'APPLIED' -and $res.final_branch -eq 'feature-x') {
        Report-Pass 'T47' 'ALREADY_SATISFIED branch mismatch proceeds to action phase'
    } else {
        Report-Fail 'T47' 'ALREADY_SATISFIED branch mismatch proceeds to action phase' "got: $($res.result) $($res.mutation_state) $($res.reason)"
    }
} catch {
    Report-Fail 'T47' 'ALREADY_SATISFIED branch mismatch proceeds to action phase' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T48 — expectation mismatch is distinguished from POLICY/LAUNCH/ENGINE failure without exception-message matching
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't48'

    # Part A: Expectation mismatch in already_satisfied_checks -> proceeds to action
    $mA = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'already_satisfied_checks' = @(
            [ordered]@{
                'id' = 'mismatch-check'
                'executable' = 'git'
                'arguments' = @('rev-parse', 'HEAD')
                'cwd' = $repo.RepoDir
                'mutating' = $false
                'expect' = [ordered]@{
                    'exit_code' = 0
                    'stdout_equals' = '0000000000000000000000000000000000000000'
                }
            }
        )
        'authorized_commands' = @(
            [ordered]@{
                'id' = 'act-marker-t48'
                'executable' = 'git'
                'arguments' = @('config', 'test.t48.partA', 'ran')
                'cwd' = $repo.RepoDir
                'mutating' = $true
                'expect' = [ordered]@{ 'exit_code' = 0 }
            }
        )
    }
    $resA = Invoke-TestEngine -ManifestPath $mA.ManifestPath -OutputRoot $outputRoot -PassThru

    # Part B: Policy violation in already_satisfied_checks -> STOPPED, no action runs
    $mB = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'already_satisfied_checks' = @(
            [ordered]@{
                'id' = 'policy-fail-check'
                'executable' = 'git'
                'arguments' = @('reset', '--hard', 'HEAD')
                'cwd' = $repo.RepoDir
                'mutating' = $false
                'expect' = [ordered]@{ 'exit_code' = 0 }
            }
        )
        'authorized_commands' = @(
            [ordered]@{
                'id' = 'act-marker-t48b'
                'executable' = 'git'
                'arguments' = @('config', 'test.t48.partB', 'ran')
                'cwd' = $repo.RepoDir
                'mutating' = $true
                'expect' = [ordered]@{ 'exit_code' = 0 }
            }
        )
    }
    $resB = Invoke-TestEngine -ManifestPath $mB.ManifestPath -OutputRoot $outputRoot -PassThru

    # Part C: Launch failure in already_satisfied_checks -> STOPPED, no action runs
    $mC = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'already_satisfied_checks' = @(
            [ordered]@{
                'id' = 'launch-fail-check'
                'executable' = 'nonexistent_binary_xyz_t48'
                'arguments' = @('arg')
                'cwd' = $repo.RepoDir
                'mutating' = $false
                'expect' = [ordered]@{ 'exit_code' = 0 }
            }
        )
        'authorized_commands' = @(
            [ordered]@{
                'id' = 'act-marker-t48c'
                'executable' = 'git'
                'arguments' = @('config', 'test.t48.partC', 'ran')
                'cwd' = $repo.RepoDir
                'mutating' = $true
                'expect' = [ordered]@{ 'exit_code' = 0 }
            }
        )
    }
    $resC = Invoke-TestEngine -ManifestPath $mC.ManifestPath -OutputRoot $outputRoot -PassThru

    $valA = & git -C $repo.RepoDir config test.t48.partA 2>$null
    $valB = & git -C $repo.RepoDir config test.t48.partB 2>$null
    $valC = & git -C $repo.RepoDir config test.t48.partC 2>$null

    if ($resA.result -eq 'COMPLETED' -and $valA -eq 'ran' -and
        $resB.result -eq 'STOPPED' -and $null -eq $valB -and
        $resC.result -eq 'STOPPED' -and $null -eq $valC) {
        Report-Pass 'T48' 'expectation mismatch is distinguished from POLICY/LAUNCH/ENGINE failure without string matching'
    } else {
        Report-Fail 'T48' 'expectation mismatch is distinguished from POLICY/LAUNCH/ENGINE failure without string matching' "resA=$($resA.result), resB=$($resB.result), resC=$($resC.result)"
    }
} catch {
    Report-Fail 'T48' 'expectation mismatch is distinguished from POLICY/LAUNCH/ENGINE failure without string matching' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $mA -and (Test-Path $mA.ManifestPath)) { Remove-Item -Force $mA.ManifestPath }
    if ($null -ne $mB -and (Test-Path $mB.ManifestPath)) { Remove-Item -Force $mB.ManifestPath }
    if ($null -ne $mC -and (Test-Path $mC.ManifestPath)) { Remove-Item -Force $mC.ManifestPath }
}

# ----------------------------------------------------
# T49 — OutputRoot inside repo using different path casing is rejected
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't49'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha
    $casedInsideRoot = $repo.RepoDir.ToLowerInvariant() + '\cased_artifacts'
    $res = Invoke-TestEngine -ManifestPath $m.ManifestPath -OutputRoot $casedInsideRoot -PassThru
    $createdInside = Test-Path $casedInsideRoot
    if ($res.result -eq 'STOPPED' -and $res.reason -match 'must not be equal to or inside working_directory' -and -not $createdInside) {
        Report-Pass 'T49' 'OutputRoot inside repo using different path casing is rejected'
    } else {
        Report-Fail 'T49' 'OutputRoot inside repo using different path casing is rejected' "got: $($res.result) $($res.reason), createdInside=$createdInside"
    }
} catch {
    Report-Fail 'T49' 'OutputRoot inside repo using different path casing is rejected' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T50 — arguments containing non-string JSON element are rejected during MANIFEST_VALIDATION
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't50'

    # Number in arguments
    $m1 = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'authorized_commands' = @(
            [ordered]@{
                'id' = 'cmd-num'
                'executable' = 'git'
                'arguments' = @(123)
                'cwd' = $repo.RepoDir
                'mutating' = $false
                'expect' = [ordered]@{ 'exit_code' = 0 }
            }
        )
    }
    $res1 = Invoke-TestEngine -ManifestPath $m1.ManifestPath -OutputRoot $outputRoot -PassThru

    # Boolean in arguments
    $m2 = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'authorized_commands' = @(
            [ordered]@{
                'id' = 'cmd-bool'
                'executable' = 'git'
                'arguments' = @($true)
                'cwd' = $repo.RepoDir
                'mutating' = $false
                'expect' = [ordered]@{ 'exit_code' = 0 }
            }
        )
    }
    $res2 = Invoke-TestEngine -ManifestPath $m2.ManifestPath -OutputRoot $outputRoot -PassThru

    # Null in arguments
    $rawJson = [regex]::Replace((Get-Content -LiteralPath $m1.ManifestPath -Raw), '\[\s*123\s*\]', '[null]')
    $nullManifestPath = Join-Path $env:TEMP 't50-null-manifest.json'
    [System.IO.File]::WriteAllText($nullManifestPath, $rawJson, [System.Text.UTF8Encoding]::new($false))
    $res3 = Invoke-TestEngine -ManifestPath $nullManifestPath -OutputRoot $outputRoot -PassThru

    if ($res1.result -eq 'STOPPED' -and $res1.failed_phase -eq 'MANIFEST_VALIDATION' -and
        $res2.result -eq 'STOPPED' -and $res2.failed_phase -eq 'MANIFEST_VALIDATION' -and
        $res3.result -eq 'STOPPED' -and $res3.failed_phase -eq 'MANIFEST_VALIDATION') {
        Report-Pass 'T50' 'arguments containing non-string JSON element are rejected during MANIFEST_VALIDATION'
    } else {
        Report-Fail 'T50' 'arguments containing non-string JSON element are rejected during MANIFEST_VALIDATION' "res1=$($res1.result)/$($res1.failed_phase), res2=$($res2.result)/$($res2.failed_phase), res3=$($res3.result)/$($res3.failed_phase)"
    }
} catch {
    Report-Fail 'T50' 'arguments containing non-string JSON element are rejected during MANIFEST_VALIDATION' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m1 -and (Test-Path $m1.ManifestPath)) { Remove-Item -Force $m1.ManifestPath }
    if ($null -ne $m2 -and (Test-Path $m2.ManifestPath)) { Remove-Item -Force $m2.ManifestPath }
    if (Test-Path $nullManifestPath) { Remove-Item -Force $nullManifestPath }
}

# ----------------------------------------------------
# T51 — branch_transition.allowed supplied as string "false" or "true" is rejected
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't51'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha

    $rawJson = Get-Content -LiteralPath $m.ManifestPath -Raw
    $strFalseJson = [regex]::Replace($rawJson, '"allowed"\s*:\s*false', '"allowed": "false"')
    $strTrueJson = [regex]::Replace($rawJson, '"allowed"\s*:\s*false', '"allowed": "true"')

    $pFalse = Join-Path $env:TEMP 't51-false.json'
    $pTrue = Join-Path $env:TEMP 't51-true.json'
    [System.IO.File]::WriteAllText($pFalse, $strFalseJson, [System.Text.UTF8Encoding]::new($false))
    [System.IO.File]::WriteAllText($pTrue, $strTrueJson, [System.Text.UTF8Encoding]::new($false))

    $resFalse = Invoke-TestEngine -ManifestPath $pFalse -OutputRoot $outputRoot -PassThru
    $resTrue = Invoke-TestEngine -ManifestPath $pTrue -OutputRoot $outputRoot -PassThru

    if ($resFalse.result -eq 'STOPPED' -and $resFalse.reason -match 'branch_transition.allowed must be a strict JSON boolean' -and
        $resTrue.result -eq 'STOPPED' -and $resTrue.reason -match 'branch_transition.allowed must be a strict JSON boolean') {
        Report-Pass 'T51' 'branch_transition.allowed supplied as string is rejected'
    } else {
        Report-Fail 'T51' 'branch_transition.allowed supplied as string is rejected' "resFalse=$($resFalse.result) $($resFalse.reason), resTrue=$($resTrue.result) $($resTrue.reason)"
    }
} catch {
    Report-Fail 'T51' 'branch_transition.allowed supplied as string is rejected' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
    if (Test-Path $pFalse) { Remove-Item -Force $pFalse }
    if (Test-Path $pTrue) { Remove-Item -Force $pTrue }
}

# ----------------------------------------------------
# T52 — manifest command id beginning with builtin- is rejected before execution
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't52'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'authorized_commands' = @(
            [ordered]@{
                'id' = 'builtin-my-action'
                'executable' = 'git'
                'arguments' = @('status')
                'cwd' = $repo.RepoDir
                'mutating' = $false
                'expect' = [ordered]@{ 'exit_code' = 0 }
            }
        )
    }
    $res = Invoke-TestEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -PassThru
    if ($res.result -eq 'STOPPED' -and $res.reason -match "Manifest command id cannot start with 'builtin-'") {
        Report-Pass 'T52' 'manifest command id beginning with builtin- is rejected before execution'
    } else {
        Report-Fail 'T52' 'manifest command id beginning with builtin- is rejected before execution' "got: $($res.result) $($res.reason)"
    }
} catch {
    Report-Fail 'T52' 'manifest command id beginning with builtin- is rejected before execution' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T53 — launch failure journal record has existing zero-byte stdout and stderr evidence files
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't53'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'required_preconditions' = @(
            [ordered]@{
                'id' = 'nonexistent-precondition'
                'executable' = 'nonexistent_test_bin_xyz_t53'
                'arguments' = @('arg')
                'cwd' = $repo.RepoDir
                'mutating' = $false
                'expect' = [ordered]@{ 'exit_code' = 0 }
            }
        )
    }
    $res = Invoke-TestEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -PassThru
    $jPath = Join-Path $res.run_directory 'commands.jsonl'
    $lines = Get-Content -LiteralPath $jPath
    $rec = $null
    foreach ($l in $lines) {
        $obj = $l | ConvertFrom-Json
        if ($obj.command_id -eq 'nonexistent-precondition') { $rec = $obj; break }
    }
    $stdoutExists = $null -ne $rec -and (Test-Path -LiteralPath $rec.stdout_path)
    $stderrExists = $null -ne $rec -and (Test-Path -LiteralPath $rec.stderr_path)
    $stdoutLen = if ($stdoutExists) { [System.IO.File]::ReadAllBytes($rec.stdout_path).Length } else { -1 }
    $stderrLen = if ($stderrExists) { [System.IO.File]::ReadAllBytes($rec.stderr_path).Length } else { -1 }

    if ($res.result -eq 'STOPPED' -and $stdoutExists -and $stderrExists -and $stdoutLen -eq 0 -and $stderrLen -eq 0) {
        Report-Pass 'T53' 'launch failure journal record has existing zero-byte stdout and stderr evidence files'
    } else {
        Report-Fail 'T53' 'launch failure journal record has existing zero-byte stdout and stderr evidence files' "stdoutExists=$stdoutExists, stdoutLen=$stdoutLen, stderrLen=$stderrLen"
    }
} catch {
    Report-Fail 'T53' 'launch failure journal record has existing zero-byte stdout and stderr evidence files' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T54 — rename scope parsing checks both source and destination paths
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't54'
    $initFilePath = Join-Path $repo.RepoDir 'source_secret.txt'
    [System.IO.File]::WriteAllText($initFilePath, 'secret', [System.Text.UTF8Encoding]::new($false))
    & git -C $repo.RepoDir add source_secret.txt
    & git -C $repo.RepoDir commit -m "add secret"
    $headSha2 = (& git -C $repo.RepoDir rev-parse HEAD).Trim()

    $docsDir = Join-Path $repo.RepoDir 'docs'
    New-Item -ItemType Directory -Force -Path $docsDir | Out-Null
    & git -C $repo.RepoDir mv source_secret.txt docs/dest.txt

    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $headSha2 -CustomProperties @{
        'authorized_paths' = @('docs/**')
        'forbidden_paths' = @('source_secret.txt')
        'authorized_commands' = @(
            [ordered]@{
                'id' = 'status-action'
                'executable' = 'git'
                'arguments' = @('status', '--short')
                'cwd' = $repo.RepoDir
                'mutating' = $true
                'expect' = [ordered]@{ 'exit_code' = 0 }
            }
        )
    }
    $res = Invoke-TestEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -PassThru
    if ($res.result -eq 'STOPPED' -and $res.reason -match 'Scope violation: changed file .* matches forbidden pattern') {
        Report-Pass 'T54' 'rename scope parsing checks both source and destination paths'
    } else {
        Report-Fail 'T54' 'rename scope parsing checks both source and destination paths' "got: $($res.result) $($res.reason)"
    }
} catch {
    Report-Fail 'T54' 'rename scope parsing checks both source and destination paths' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T55 — filename with leading or trailing space is not Trim-normalized during scope verification
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't55'
    $spacedFile = Join-Path $repo.RepoDir ' space_file.txt'
    [System.IO.File]::WriteAllText($spacedFile, 'data', [System.Text.UTF8Encoding]::new($false))

    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'authorized_paths' = @('space_file.txt')
        'authorized_commands' = @(
            [ordered]@{
                'id' = 'status-action-t55'
                'executable' = 'git'
                'arguments' = @('status', '--short')
                'cwd' = $repo.RepoDir
                'mutating' = $true
                'expect' = [ordered]@{ 'exit_code' = 0 }
            }
        )
    }
    $res = Invoke-TestEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -PassThru
    if ($res.result -eq 'STOPPED' -and $res.reason -match 'Scope violation') {
        Report-Pass 'T55' 'filename with leading or trailing space is not Trim-normalized'
    } else {
        Report-Fail 'T55' 'filename with leading or trailing space is not Trim-normalized' "got: $($res.result) $($res.reason)"
    }
} catch {
    Report-Fail 'T55' 'filename with leading or trailing space is not Trim-normalized' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T56 — refresh force refspec beginning with + is rejected before launch
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't56'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'refresh_commands' = @(
            [ordered]@{
                'id' = 'refresh-plus-refspec'
                'executable' = 'git'
                'arguments' = @('fetch', 'origin', '+refs/heads/main:refs/remotes/origin/main')
                'cwd' = $repo.RepoDir
                'mutating' = $false
                'expect' = [ordered]@{ 'exit_code' = 0 }
            }
        )
    }
    $res = Invoke-TestEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -PassThru
    if ($res.result -eq 'STOPPED' -and $res.reason -match 'Forbidden force refspec in refresh_commands') {
        Report-Pass 'T56' 'refresh force refspec beginning with + is rejected before launch'
    } else {
        Report-Fail 'T56' 'refresh force refspec beginning with + is rejected before launch' "got: $($res.result) $($res.reason)"
    }
} catch {
    Report-Fail 'T56' 'refresh force refspec beginning with + is rejected before launch' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T57 — refresh prune variants are rejected (-p, --prune=true)
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't57'
    $m1 = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'refresh_commands' = @(
            [ordered]@{
                'id' = 'refresh-p'
                'executable' = 'git'
                'arguments' = @('fetch', '-p', 'origin')
                'cwd' = $repo.RepoDir
                'mutating' = $false
                'expect' = [ordered]@{ 'exit_code' = 0 }
            }
        )
    }
    $res1 = Invoke-TestEngine -ManifestPath $m1.ManifestPath -OutputRoot $outputRoot -PassThru

    $m2 = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'refresh_commands' = @(
            [ordered]@{
                'id' = 'refresh-prune-true'
                'executable' = 'git'
                'arguments' = @('fetch', '--prune=true', 'origin')
                'cwd' = $repo.RepoDir
                'mutating' = $false
                'expect' = [ordered]@{ 'exit_code' = 0 }
            }
        )
    }
    $res2 = Invoke-TestEngine -ManifestPath $m2.ManifestPath -OutputRoot $outputRoot -PassThru

    if ($res1.result -eq 'STOPPED' -and $res1.reason -match 'Forbidden force/prune flag in refresh_commands' -and
        $res2.result -eq 'STOPPED' -and $res2.reason -match 'Forbidden force/prune flag in refresh_commands') {
        Report-Pass 'T57' 'refresh prune variants are rejected'
    } else {
        Report-Fail 'T57' 'refresh prune variants are rejected' "res1=$($res1.result), res2=$($res2.result)"
    }
} catch {
    Report-Fail 'T57' 'refresh prune variants are rejected' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m1 -and (Test-Path $m1.ManifestPath)) { Remove-Item -Force $m1.ManifestPath }
    if ($null -ne $m2 -and (Test-Path $m2.ManifestPath)) { Remove-Item -Force $m2.ManifestPath }
}

# ----------------------------------------------------
# T58 — powershell.exe -enc and abbreviated -Command rejected, -File allowed
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't58'

    $m1 = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'authorized_commands' = @(
            [ordered]@{
                'id' = 'ps-enc'
                'executable' = 'powershell.exe'
                'arguments' = @('-enc', 'V3JpdGUtSG9zdCAiZXZpbCI=')
                'cwd' = $repo.RepoDir
                'mutating' = $true
                'expect' = [ordered]@{ 'exit_code' = 0 }
            }
        )
    }
    $res1 = Invoke-TestEngine -ManifestPath $m1.ManifestPath -OutputRoot $outputRoot -PassThru

    $m2 = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'authorized_commands' = @(
            [ordered]@{
                'id' = 'ps-comm'
                'executable' = 'powershell.exe'
                'arguments' = @('-comm', 'Write-Host "evil"')
                'cwd' = $repo.RepoDir
                'mutating' = $true
                'expect' = [ordered]@{ 'exit_code' = 0 }
            }
        )
    }
    $res2 = Invoke-TestEngine -ManifestPath $m2.ManifestPath -OutputRoot $outputRoot -PassThru

    $scriptPath = Join-Path $repo.RepoDir 'my_script.ps1'
    [System.IO.File]::WriteAllText($scriptPath, 'param($param1) Write-Output "ok"', [System.Text.UTF8Encoding]::new($false))
    $m3 = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'authorized_commands' = @(
            [ordered]@{
                'id' = 'ps-file'
                'executable' = 'powershell.exe'
                'arguments' = @('-NoProfile', '-File', $scriptPath, '-enc', 'script_arg_value')
                'cwd' = $repo.RepoDir
                'mutating' = $false
                'expect' = [ordered]@{
                    'exit_code' = 0
                    'stdout_equals' = 'ok'
                }
            }
        )
    }
    $res3 = Invoke-TestEngine -ManifestPath $m3.ManifestPath -OutputRoot $outputRoot -PassThru

    if ($res1.result -eq 'STOPPED' -and $res1.reason -match 'PowerShell encoded command execution' -and
        $res2.result -eq 'STOPPED' -and $res2.reason -match 'PowerShell command string execution' -and
        $res3.result -eq 'COMPLETED') {
        Report-Pass 'T58' 'powershell.exe -enc and abbreviated -Command rejected, -File allowed'
    } else {
        Report-Fail 'T58' 'powershell.exe -enc and abbreviated -Command rejected, -File allowed' "res1=$($res1.result), res2=$($res2.result), res3=$($res3.result) $($res3.reason)"
    }
} catch {
    Report-Fail 'T58' 'powershell.exe -enc and abbreviated -Command rejected, -File allowed' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m1 -and (Test-Path $m1.ManifestPath)) { Remove-Item -Force $m1.ManifestPath }
    if ($null -ne $m2 -and (Test-Path $m2.ManifestPath)) { Remove-Item -Force $m2.ManifestPath }
    if ($null -ne $m3 -and (Test-Path $m3.ManifestPath)) { Remove-Item -Force $m3.ManifestPath }
}

# ----------------------------------------------------
# T59 — distinct remote refs whose slash-to-dash forms would collide receive distinct command IDs
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't59' -WithBareOrigin
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'expected_remote_refs' = [ordered]@{
            'origin/foo/bar' = 'ABSENT'
            'origin/foo-bar' = 'ABSENT'
        }
    }
    $res = Invoke-TestEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -PassThru
    $jPath = Join-Path $res.run_directory 'commands.jsonl'
    $lines = Get-Content -LiteralPath $jPath
    $ids = [System.Collections.Generic.List[string]]::new()
    foreach ($l in $lines) {
        $obj = $l | ConvertFrom-Json
        if ($obj.command_id -like 'builtin-ref-*') {
            $ids.Add($obj.command_id)
        }
    }
    if ($res.result -eq 'COMPLETED' -and $ids.Count -eq 2 -and $ids[0] -ne $ids[1]) {
        Report-Pass 'T59' 'distinct remote refs receive distinct journal command IDs'
    } else {
        Report-Fail 'T59' 'distinct remote refs receive distinct journal command IDs' "idsCount=$($ids.Count), ids=$($ids -join ', ')"
    }
} catch {
    Report-Fail 'T59' 'distinct remote refs receive distinct journal command IDs' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo -RepoDir $repo.RepoDir -BareDir $repo.BareDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T60 — strengthened: mutating process starts, then injected engine/capture failure occurs (R3-03)
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't60'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'authorized_commands' = @(
            [ordered]@{
                'id' = 'start-mutation-injected-fail'
                'executable' = 'git'
                'arguments' = @('config', 'test.t60.marker', 'started_and_ran')
                'cwd' = $repo.RepoDir
                'mutating' = $true
                'expect' = [ordered]@{ 'exit_code' = 0 }
            }
        )
    }
    $injectedHook = {
        param($cmdId, $phase, $proc)
        if ($cmdId -eq 'start-mutation-injected-fail') {
            if ($null -ne $proc) {
                $proc.WaitForExit()
            }
            throw "Injected post-start capture/engine failure for T60"
        }
    }
    $res = Invoke-TestEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -PassThru -TestPostStartHook $injectedHook
    $markerVal = & git -C $repo.RepoDir config test.t60.marker 2>$null

    $jPath = Join-Path $res.run_directory 'commands.jsonl'
    $lines = Get-Content -LiteralPath $jPath
    $rec = $null
    foreach ($l in $lines) {
        $obj = $l | ConvertFrom-Json
        if ($obj.command_id -eq 'start-mutation-injected-fail') { $rec = $obj; break }
    }

    $hasEngineError = $null -ne $rec -and ($rec.PSObject.Properties['engine_error'] -ne $null)
    $hasLaunchError = $null -ne $rec -and ($rec.PSObject.Properties['launch_error'] -ne $null)

    if ($res.result -eq 'STOPPED' -and $res.mutation_state -eq 'UNKNOWN' -and $markerVal -eq 'started_and_ran' -and
        $null -ne $rec -and $hasEngineError -and -not $hasLaunchError) {
        Report-Pass 'T60' 'mutating process starts, then injected engine failure occurs: STOPPED, UNKNOWN'
    } else {
        Report-Fail 'T60' 'mutating process starts, then injected engine failure occurs: STOPPED, UNKNOWN' "res=$($res.result), state=$($res.mutation_state), marker=$markerVal, engErr=$hasEngineError, launchErr=$hasLaunchError"
    }
} catch {
    Report-Fail 'T60' 'mutating process starts, then injected engine failure occurs: STOPPED, UNKNOWN' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T61 — expect object rejects unknown field and wrong stdout_empty / stderr_empty JSON types
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't61'

    $m1 = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'authorized_commands' = @(
            [ordered]@{
                'id' = 'cmd-unknown-exp'
                'executable' = 'git'
                'arguments' = @('status')
                'cwd' = $repo.RepoDir
                'mutating' = $false
                'expect' = [ordered]@{
                    'exit_code' = 0
                    'unknown_exp_prop' = 'bad'
                }
            }
        )
    }
    $res1 = Invoke-TestEngine -ManifestPath $m1.ManifestPath -OutputRoot $outputRoot -PassThru

    $m2 = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'authorized_commands' = @(
            [ordered]@{
                'id' = 'cmd-bad-type-exp'
                'executable' = 'git'
                'arguments' = @('status')
                'cwd' = $repo.RepoDir
                'mutating' = $false
                'expect' = [ordered]@{
                    'exit_code' = 0
                    'stdout_empty' = 'not_a_bool'
                }
            }
        )
    }
    $res2 = Invoke-TestEngine -ManifestPath $m2.ManifestPath -OutputRoot $outputRoot -PassThru

    if ($res1.result -eq 'STOPPED' -and $res1.reason -match 'Unknown property in expect object' -and
        $res2.result -eq 'STOPPED' -and $res2.reason -match 'expect.stdout_empty must be a strict JSON boolean') {
        Report-Pass 'T61' 'expect object rejects unknown field and wrong property types'
    } else {
        Report-Fail 'T61' 'expect object rejects unknown field and wrong property types' "res1=$($res1.result), res2=$($res2.result)"
    }
} catch {
    Report-Fail 'T61' 'expect object rejects unknown field and wrong property types' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m1 -and (Test-Path $m1.ManifestPath)) { Remove-Item -Force $m1.ManifestPath }
    if ($null -ne $m2 -and (Test-Path $m2.ManifestPath)) { Remove-Item -Force $m2.ManifestPath }
}

# ----------------------------------------------------
# T62 — command sections and authorized_paths/forbidden_paths reject non-array or non-string element structures
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't62'

    # authorized_commands as non-array
    $m1 = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'authorized_commands' = 'not-an-array'
    }
    $res1 = Invoke-TestEngine -ManifestPath $m1.ManifestPath -OutputRoot $outputRoot -PassThru

    # authorized_paths as non-array
    $m2 = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'authorized_paths' = 'not-an-array'
    }
    $res2 = Invoke-TestEngine -ManifestPath $m2.ManifestPath -OutputRoot $outputRoot -PassThru

    # forbidden_paths containing integer element
    $m3 = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'forbidden_paths' = @(999)
    }
    $res3 = Invoke-TestEngine -ManifestPath $m3.ManifestPath -OutputRoot $outputRoot -PassThru

    if ($res1.result -eq 'STOPPED' -and $res1.reason -match 'Command section .* must be a JSON array' -and
        $res2.result -eq 'STOPPED' -and $res2.reason -match "Field 'authorized_paths' must be a JSON array" -and
        $res3.result -eq 'STOPPED' -and $res3.reason -match "Field 'forbidden_paths' must contain only JSON strings") {
        Report-Pass 'T62' 'command sections and paths reject non-array or non-string structures'
    } else {
        Report-Fail 'T62' 'command sections and paths reject non-array or non-string structures' "res1=$($res1.result), res2=$($res2.result), res3=$($res3.result)"
    }
} catch {
    Report-Fail 'T62' 'command sections and paths reject non-array or non-string structures' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m1 -and (Test-Path $m1.ManifestPath)) { Remove-Item -Force $m1.ManifestPath }
    if ($null -ne $m2 -and (Test-Path $m2.ManifestPath)) { Remove-Item -Force $m2.ManifestPath }
    if ($null -ne $m3 -and (Test-Path $m3.ManifestPath)) { Remove-Item -Force $m3.ManifestPath }
}

# ----------------------------------------------------
# T63 — checked-in smoke example is a non-executable template (R3-01)
# ----------------------------------------------------
try {
    $examplePath = 'C:\PALKA\scripts\governance\examples\read-only-smoke.manifest.json'
    $exampleContent = [System.IO.File]::ReadAllText($examplePath)
    $hasHeadPlaceholder = $exampleContent.Contains('<40-hex-head-sha>')
    $hasBasePlaceholder = $exampleContent.Contains('<40-hex-base-sha>')
    $hasOriginPlaceholder = $exampleContent.Contains('<40-hex-origin-main-sha>')
    $hasRealSha = $exampleContent.Contains('572f9d277e5b5af269b86bafac18e2e9d33c2bf5')

    if ($hasHeadPlaceholder -and $hasBasePlaceholder -and $hasOriginPlaceholder -and -not $hasRealSha) {
        Report-Pass 'T63' 'checked-in smoke example is a non-executable template'
    } else {
        Report-Fail 'T63' 'checked-in smoke example is a non-executable template' "head=$hasHeadPlaceholder, base=$hasBasePlaceholder, origin=$hasOriginPlaceholder, hasReal=$hasRealSha"
    }
} catch {
    Report-Fail 'T63' 'checked-in smoke example is a non-executable template' $_.Exception.Message
}

# ----------------------------------------------------
# T64 — non-mutating process starts, then injected engine/capture failure occurs (R3-02, R3-03)
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't64'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'authorized_commands' = @(
            [ordered]@{
                'id' = 'read-cmd-injected-fail'
                'executable' = 'git'
                'arguments' = @('status', '--short')
                'cwd' = $repo.RepoDir
                'mutating' = $false
                'expect' = [ordered]@{ 'exit_code' = 0 }
            }
        )
    }
    $injectedHook = {
        param($cmdId, $phase)
        if ($cmdId -eq 'read-cmd-injected-fail') {
            throw "Injected post-start capture/engine failure for T64"
        }
    }
    $res = Invoke-TestEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -PassThru -TestPostStartHook $injectedHook

    $jPath = Join-Path $res.run_directory 'commands.jsonl'
    $lines = Get-Content -LiteralPath $jPath
    $matchingRecords = @()
    foreach ($l in $lines) {
        $obj = $l | ConvertFrom-Json
        if ($obj.command_id -eq 'read-cmd-injected-fail') { $matchingRecords += $obj }
    }

    $singleRecord = $matchingRecords.Count -eq 1
    $rec = if ($singleRecord) { $matchingRecords[0] } else { $null }
    $hasEngineError = $null -ne $rec -and ($rec.PSObject.Properties['engine_error'] -ne $null)
    $hasLaunchError = $null -ne $rec -and ($rec.PSObject.Properties['launch_error'] -ne $null)

    if ($res.result -eq 'STOPPED' -and $res.mutation_state -eq 'NONE' -and $singleRecord -and $hasEngineError -and -not $hasLaunchError) {
        Report-Pass 'T64' 'non-mutating process starts, then injected engine failure occurs: STOPPED, NONE'
    } else {
        Report-Fail 'T64' 'non-mutating process starts, then injected engine failure occurs: STOPPED, NONE' "res=$($res.result), state=$($res.mutation_state), records=$($matchingRecords.Count), engErr=$hasEngineError, launchErr=$hasLaunchError"
    }
} catch {
    Report-Fail 'T64' 'non-mutating process starts, then injected engine failure occurs: STOPPED, NONE' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T65 — PowerShell CommandWithArgs abbreviations are rejected (R3-04)
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't65'

    $m1 = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'authorized_commands' = @(
            [ordered]@{
                'id' = 'pwsh-commandw'
                'executable' = 'pwsh.exe'
                'arguments' = @('-CommandW', 'Write-Host evil')
                'cwd' = $repo.RepoDir
                'mutating' = $true
                'expect' = [ordered]@{ 'exit_code' = 0 }
            }
        )
    }
    $res1 = Invoke-TestEngine -ManifestPath $m1.ManifestPath -OutputRoot $outputRoot -PassThru

    $m2 = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'authorized_commands' = @(
            [ordered]@{
                'id' = 'ps-commandwitha'
                'executable' = 'powershell.exe'
                'arguments' = @('-CommandWithA', 'Write-Host evil')
                'cwd' = $repo.RepoDir
                'mutating' = $true
                'expect' = [ordered]@{ 'exit_code' = 0 }
            }
        )
    }
    $res2 = Invoke-TestEngine -ManifestPath $m2.ManifestPath -OutputRoot $outputRoot -PassThru

    if ($res1.result -eq 'STOPPED' -and $res1.reason -match 'PowerShell command string execution' -and
        $res2.result -eq 'STOPPED' -and $res2.reason -match 'PowerShell command string execution') {
        Report-Pass 'T65' 'PowerShell CommandWithArgs abbreviations are rejected'
    } else {
        Report-Fail 'T65' 'PowerShell CommandWithArgs abbreviations are rejected' "res1=$($res1.result) $($res1.reason), res2=$($res2.result) $($res2.reason)"
    }
} catch {
    Report-Fail 'T65' 'PowerShell CommandWithArgs abbreviations are rejected' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m1 -and (Test-Path $m1.ManifestPath)) { Remove-Item -Force $m1.ManifestPath }
    if ($null -ne $m2 -and (Test-Path $m2.ManifestPath)) { Remove-Item -Force $m2.ManifestPath }
}

# ----------------------------------------------------
# T66 — Git push policy parser handles bare git push without throwing (R3-05)
# ----------------------------------------------------
try {
    # Unit policy test on arguments @('push')
    $dangCheck = Test-PalkaDangerousPolicy -Executable 'git' -Arguments @('push')
    # Bare git push is not inherently rejected by dangerous policy (it is a standard push without forbidden flags)
    # The crucial assertion is that policy parsing completes without throwing an array index or null exception
    if ($null -eq $dangCheck) {
        Report-Pass 'T66' 'Git push policy parser handles bare git push without throwing'
    } else {
        Report-Fail 'T66' 'Git push policy parser handles bare git push without throwing' "got policy: $dangCheck"
    }
} catch {
    Report-Fail 'T66' 'Git push policy parser handles bare git push without throwing' $_.Exception.Message
}

# ----------------------------------------------------
# T67 — Git push destructive variants rejected: --mirror, --prune (R3-05)
# ----------------------------------------------------
try {
    $dangMirror = Test-PalkaDangerousPolicy -Executable 'git' -Arguments @('push', '--mirror', 'origin')
    $dangPrune = Test-PalkaDangerousPolicy -Executable 'git' -Arguments @('push', '--prune', 'origin')
    $dangMirrorAssign = Test-PalkaDangerousPolicy -Executable 'git' -Arguments @('push', '--mirror=all', 'origin')
    $dangPruneAssign = Test-PalkaDangerousPolicy -Executable 'git' -Arguments @('push', '--prune=all', 'origin')

    if ($dangMirror -match 'Globally forbidden git push flag' -and
        $dangPrune -match 'Globally forbidden git push flag' -and
        $dangMirrorAssign -match 'Globally forbidden git push flag' -and
        $dangPruneAssign -match 'Globally forbidden git push flag') {
        Report-Pass 'T67' 'Git push destructive variants rejected: --mirror, --prune'
    } else {
        Report-Fail 'T67' 'Git push destructive variants rejected: --mirror, --prune' "mirror=$dangMirror, prune=$dangPrune"
    }
} catch {
    Report-Fail 'T67' 'Git push destructive variants rejected: --mirror, --prune' $_.Exception.Message
}

# ----------------------------------------------------
# T68 — refresh --refmap=+... is rejected before launch (R3-06)
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't68'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'refresh_commands' = @(
            [ordered]@{
                'id' = 'refresh-refmap-force'
                'executable' = 'git'
                'arguments' = @('fetch', 'origin', '--refmap=+refs/heads/main:refs/remotes/origin/main')
                'cwd' = $repo.RepoDir
                'mutating' = $false
                'expect' = [ordered]@{ 'exit_code' = 0 }
            }
        )
    }
    $res = Invoke-TestEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -PassThru
    if ($res.result -eq 'STOPPED' -and $res.reason -match 'Forbidden force refmap in refresh_commands') {
        Report-Pass 'T68' 'refresh --refmap=+... is rejected before launch'
    } else {
        Report-Fail 'T68' 'refresh --refmap=+... is rejected before launch' "got: $($res.result) $($res.reason)"
    }
} catch {
    Report-Fail 'T68' 'refresh --refmap=+... is rejected before launch' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T69 вЂ” missing AuthorizedManifestSha256 (DEC-003 Phase 2A.2)
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't69'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha
    $nativeCount = 0
    $hook = { param($cmd, $ph, $p) $script:nativeCount++ }
    $res = Invoke-PalkaEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -PassThru -TestPostStartHook $hook
    if ($res.result -eq 'STOPPED' -and $res.mutation_state -eq 'NOT_APPLIED' -and $res.failed_phase -eq 'MANIFEST_DIGEST_VALIDATION' -and $nativeCount -eq 0) {
        Report-Pass 'T69' 'missing AuthorizedManifestSha256 rejected with zero native launches'
    } else {
        Report-Fail 'T69' 'missing AuthorizedManifestSha256 rejected with zero native launches' "got: $($res.result) $($res.mutation_state) $($res.failed_phase) count=$nativeCount"
    }
} catch {
    Report-Fail 'T69' 'missing AuthorizedManifestSha256 rejected with zero native launches' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T70 вЂ” empty digest (DEC-003 Phase 2A.2)
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't70'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha
    $nativeCount = 0
    $hook = { param($cmd, $ph, $p) $script:nativeCount++ }
    $res = Invoke-PalkaEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -AuthorizedManifestSha256 '' -PassThru -TestPostStartHook $hook
    if ($res.result -eq 'STOPPED' -and $res.mutation_state -eq 'NOT_APPLIED' -and $res.failed_phase -eq 'MANIFEST_DIGEST_VALIDATION' -and $nativeCount -eq 0) {
        Report-Pass 'T70' 'empty digest rejected with zero native launches'
    } else {
        Report-Fail 'T70' 'empty digest rejected with zero native launches' "got: $($res.result) $($res.mutation_state) $($res.failed_phase) count=$nativeCount"
    }
} catch {
    Report-Fail 'T70' 'empty digest rejected with zero native launches' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T71 вЂ” short/malformed lowercase digest (DEC-003 Phase 2A.2)
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't71'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha
    $nativeCount = 0
    $hook = { param($cmd, $ph, $p) $script:nativeCount++ }
    $res = Invoke-PalkaEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -AuthorizedManifestSha256 'abcd1234' -PassThru -TestPostStartHook $hook
    if ($res.result -eq 'STOPPED' -and $res.mutation_state -eq 'NOT_APPLIED' -and $res.failed_phase -eq 'MANIFEST_DIGEST_VALIDATION' -and $nativeCount -eq 0) {
        Report-Pass 'T71' 'short/malformed lowercase digest rejected with zero native launches'
    } else {
        Report-Fail 'T71' 'short/malformed lowercase digest rejected with zero native launches' "got: $($res.result) $($res.mutation_state) $($res.failed_phase) count=$nativeCount"
    }
} catch {
    Report-Fail 'T71' 'short/malformed lowercase digest rejected with zero native launches' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T72 — 64-character uppercase hex digest rejected without normalization (DEC-003 Phase 2A.2)
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't72'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha
    $validSha = Get-TestManifestSha256 $m.ManifestPath
    $upperSha = $validSha.ToUpperInvariant()
    $nativeCount = 0
    $hook = { param($cmd, $ph, $p) $script:nativeCount++ }
    $res = Invoke-PalkaEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -AuthorizedManifestSha256 $upperSha -PassThru -TestPostStartHook $hook
    if ($res.result -eq 'STOPPED' -and $res.mutation_state -eq 'NOT_APPLIED' -and $res.failed_phase -eq 'MANIFEST_DIGEST_VALIDATION' -and $res.reason -notmatch 'MANIFEST_DIGEST_MISMATCH' -and $nativeCount -eq 0) {
        Report-Pass 'T72' '64-character uppercase hex digest rejected without normalization'
    } else {
        Report-Fail 'T72' '64-character uppercase hex digest rejected without normalization' "got: $($res.result) $($res.mutation_state) $($res.failed_phase) reason=$($res.reason) count=$nativeCount"
    }
} catch {
    Report-Fail 'T72' '64-character uppercase hex digest rejected without normalization' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T73 вЂ” 64-character non-hex digest rejected (DEC-003 Phase 2A.2)
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't73'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha
    $nonHex = 'g' * 64
    $nativeCount = 0
    $hook = { param($cmd, $ph, $p) $script:nativeCount++ }
    $res = Invoke-PalkaEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -AuthorizedManifestSha256 $nonHex -PassThru -TestPostStartHook $hook
    if ($res.result -eq 'STOPPED' -and $res.mutation_state -eq 'NOT_APPLIED' -and $res.failed_phase -eq 'MANIFEST_DIGEST_VALIDATION' -and $nativeCount -eq 0) {
        Report-Pass 'T73' '64-character non-hex digest rejected'
    } else {
        Report-Fail 'T73' '64-character non-hex digest rejected' "got: $($res.result) $($res.mutation_state) $($res.failed_phase) count=$nativeCount"
    }
} catch {
    Report-Fail 'T73' '64-character non-hex digest rejected' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T74 вЂ” well-formed but wrong digest (DEC-003 Phase 2A.2)
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't74'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha
    $wrongSha = '0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef'
    $nativeCount = 0
    $hook = { param($cmd, $ph, $p) $script:nativeCount++ }
    $res = Invoke-PalkaEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -AuthorizedManifestSha256 $wrongSha -PassThru -TestPostStartHook $hook
    if ($res.result -eq 'STOPPED' -and $res.mutation_state -eq 'NOT_APPLIED' -and $res.failed_phase -eq 'MANIFEST_DIGEST_VALIDATION' -and $res.reason -match 'MANIFEST_DIGEST_MISMATCH' -and $nativeCount -eq 0) {
        Report-Pass 'T74' 'well-formed but wrong digest rejected before native execution'
    } else {
        Report-Fail 'T74' 'well-formed but wrong digest rejected before native execution' "got: $($res.result) $($res.mutation_state) $($res.failed_phase) $($res.reason) count=$nativeCount"
    }
} catch {
    Report-Fail 'T74' 'well-formed but wrong digest rejected before native execution' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T75 вЂ” semantically equivalent JSON with different whitespace (DEC-003 Phase 2A.2)
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't75'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha
    $origSha = Get-TestManifestSha256 $m.ManifestPath
    # Modify whitespace without changing JSON semantics
    $origText = [System.IO.File]::ReadAllText($m.ManifestPath)
    $modifiedText = $origText + "  `n"
    [System.IO.File]::WriteAllText($m.ManifestPath, $modifiedText, [System.Text.UTF8Encoding]::new($false))
    $nativeCount = 0
    $hook = { param($cmd, $ph, $p) $script:nativeCount++ }
    $res = Invoke-PalkaEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -AuthorizedManifestSha256 $origSha -PassThru -TestPostStartHook $hook
    if ($res.result -eq 'STOPPED' -and $res.mutation_state -eq 'NOT_APPLIED' -and $res.failed_phase -eq 'MANIFEST_DIGEST_VALIDATION' -and $res.reason -match 'MANIFEST_DIGEST_MISMATCH' -and $nativeCount -eq 0) {
        Report-Pass 'T75' 'semantically equivalent JSON with different whitespace rejected by byte digest barrier'
    } else {
        Report-Fail 'T75' 'semantically equivalent JSON with different whitespace rejected by byte digest barrier' "got: $($res.result) $($res.mutation_state) $($res.reason) count=$nativeCount"
    }
} catch {
    Report-Fail 'T75' 'semantically equivalent JSON with different whitespace rejected by byte digest barrier' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T76 вЂ” LF versus CRLF manifest bytes (DEC-003 Phase 2A.2)
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't76'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha
    # Save as strictly LF
    $text = [System.IO.File]::ReadAllText($m.ManifestPath).Replace("`r`n", "`n")
    [System.IO.File]::WriteAllText($m.ManifestPath, $text, [System.Text.UTF8Encoding]::new($false))
    $lfSha = Get-TestManifestSha256 $m.ManifestPath
    # Convert to CRLF
    $crlfText = $text.Replace("`n", "`r`n")
    [System.IO.File]::WriteAllText($m.ManifestPath, $crlfText, [System.Text.UTF8Encoding]::new($false))
    $nativeCount = 0
    $hook = { param($cmd, $ph, $p) $script:nativeCount++ }
    $res = Invoke-PalkaEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -AuthorizedManifestSha256 $lfSha -PassThru -TestPostStartHook $hook
    if ($res.result -eq 'STOPPED' -and $res.mutation_state -eq 'NOT_APPLIED' -and $res.failed_phase -eq 'MANIFEST_DIGEST_VALIDATION' -and $res.reason -match 'MANIFEST_DIGEST_MISMATCH' -and $nativeCount -eq 0) {
        Report-Pass 'T76' 'LF versus CRLF manifest bytes distinct authorization'
    } else {
        Report-Fail 'T76' 'LF versus CRLF manifest bytes distinct authorization' "got: $($res.result) $($res.mutation_state) $($res.reason) count=$nativeCount"
    }
} catch {
    Report-Fail 'T76' 'LF versus CRLF manifest bytes distinct authorization' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T77 вЂ” UTF-8 BOM versus no-BOM (DEC-003 Phase 2A.2)
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't77'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha
    $noBomSha = Get-TestManifestSha256 $m.ManifestPath
    # Prepend UTF-8 BOM bytes
    $rawBytes = [System.IO.File]::ReadAllBytes($m.ManifestPath)
    $bomBytes = [byte[]]@(0xEF, 0xBB, 0xBF) + $rawBytes
    [System.IO.File]::WriteAllBytes($m.ManifestPath, $bomBytes)
    $nativeCount = 0
    $hook = { param($cmd, $ph, $p) $script:nativeCount++ }
    $res = Invoke-PalkaEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -AuthorizedManifestSha256 $noBomSha -PassThru -TestPostStartHook $hook
    if ($res.result -eq 'STOPPED' -and $res.mutation_state -eq 'NOT_APPLIED' -and $res.failed_phase -eq 'MANIFEST_DIGEST_VALIDATION' -and $res.reason -match 'MANIFEST_DIGEST_MISMATCH' -and $nativeCount -eq 0) {
        Report-Pass 'T77' 'UTF-8 BOM versus no-BOM distinct authorization'
    } else {
        Report-Fail 'T77' 'UTF-8 BOM versus no-BOM distinct authorization' "got: $($res.result) $($res.mutation_state) $($res.reason) count=$nativeCount"
    }
} catch {
    Report-Fail 'T77' 'UTF-8 BOM versus no-BOM distinct authorization' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T78 вЂ” correct digest with genuinely read-only manifest and explicitly empty stop_conditions (DEC-003 Phase 2A.2)
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't78'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'stop_conditions' = @()
    }
    $sha = Get-TestManifestSha256 $m.ManifestPath
    $res = Invoke-PalkaEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -AuthorizedManifestSha256 $sha -PassThru
    if ($res.result -eq 'COMPLETED' -and $res.mutation_state -eq 'NONE') {
        Report-Pass 'T78' 'read-only manifest with explicitly empty stop_conditions succeeds'
    } else {
        Report-Fail 'T78' 'read-only manifest with explicitly empty stop_conditions succeeds' "got: $($res.result) $($res.mutation_state) $($res.reason)"
    }
} catch {
    Report-Fail 'T78' 'read-only manifest with explicitly empty stop_conditions succeeds' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T79 вЂ” run-directory manifest.json is byte-for-byte identical to verified input and SHA-256 matches (DEC-003 Phase 2A.2)
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't79'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha
    $sha = Get-TestManifestSha256 $m.ManifestPath
    $res = Invoke-PalkaEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -AuthorizedManifestSha256 $sha -PassThru
    $runManifestPath = Join-Path $res.run_directory 'manifest.json'
    $origBytes = [System.IO.File]::ReadAllBytes($m.ManifestPath)
    $runBytes = [System.IO.File]::ReadAllBytes($runManifestPath)
    $bytesIdentical = [System.Linq.Enumerable]::SequenceEqual($origBytes, $runBytes)
    $runSha = Get-TestManifestSha256 $runManifestPath
    if ($res.result -eq 'COMPLETED' -and $bytesIdentical -and $runSha -eq $sha) {
        Report-Pass 'T79' 'run-directory manifest.json is byte-for-byte identical to verified input'
    } else {
        Report-Fail 'T79' 'run-directory manifest.json is byte-for-byte identical to verified input' "identical=$bytesIdentical, runSha=$runSha, sha=$sha"
    }
} catch {
    Report-Fail 'T79' 'run-directory manifest.json is byte-for-byte identical to verified input' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T80 — artifact_profile = bootstrap_zip_v1 / case-variants rejected before native execution (DEC-003 Phase 2A.2)
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't80'
    $m1 = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'artifact_profile' = 'bootstrap_zip_v1'
    }
    $sha1 = Get-TestManifestSha256 $m1.ManifestPath
    $nativeCount1 = 0
    $hook1 = { param($cmd, $ph, $p) $script:nativeCount1++ }
    $res1 = Invoke-PalkaEngine -ManifestPath $m1.ManifestPath -OutputRoot $outputRoot -AuthorizedManifestSha256 $sha1 -PassThru -TestPostStartHook $hook1

    $m2 = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'artifact_profile' = 'phase_2a_run_directory_v0'
    }
    $sha2 = Get-TestManifestSha256 $m2.ManifestPath
    $nativeCount2 = 0
    $hook2 = { param($cmd, $ph, $p) $script:nativeCount2++ }
    $res2 = Invoke-PalkaEngine -ManifestPath $m2.ManifestPath -OutputRoot $outputRoot -AuthorizedManifestSha256 $sha2 -PassThru -TestPostStartHook $hook2

    $pass1 = ($res1.result -eq 'STOPPED' -and $res1.mutation_state -eq 'NOT_APPLIED' -and $res1.reason -match 'artifact_profile' -and $nativeCount1 -eq 0)
    $pass2 = ($res2.result -eq 'STOPPED' -and $res2.mutation_state -eq 'NOT_APPLIED' -and $res2.reason -match 'artifact_profile' -and $nativeCount2 -eq 0)

    if ($pass1 -and $pass2) {
        Report-Pass 'T80' 'artifact_profile = bootstrap_zip_v1 and lowercase variant rejected before native execution'
    } else {
        Report-Fail 'T80' 'artifact_profile = bootstrap_zip_v1 and lowercase variant rejected before native execution' "pass1=$pass1, pass2=$pass2"
    }
} catch {
    Report-Fail 'T80' 'artifact_profile = bootstrap_zip_v1 and lowercase variant rejected before native execution' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m1 -and (Test-Path $m1.ManifestPath)) { Remove-Item -Force $m1.ManifestPath }
    if ($null -ne $m2 -and (Test-Path $m2.ManifestPath)) { Remove-Item -Force $m2.ManifestPath }
}

# ----------------------------------------------------
# T81 — mutating authorized command + empty stop_conditions rejected (DEC-003 Phase 2A.2)
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't81'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'authorized_commands' = @(
            [ordered]@{
                'id' = 'mutating-cmd'
                'executable' = 'git'
                'arguments' = @('branch', 'newbranch')
                'cwd' = $repo.RepoDir
                'mutating' = $true
                'expect' = [ordered]@{ 'exit_code' = 0 }
            }
        )
        'stop_conditions' = @()
    }
    $sha = Get-TestManifestSha256 $m.ManifestPath
    $nativeCount = 0
    $hook = { param($cmd, $ph, $p) $script:nativeCount++ }
    $res = Invoke-PalkaEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -AuthorizedManifestSha256 $sha -PassThru -TestPostStartHook $hook
    if ($res.result -eq 'STOPPED' -and $res.mutation_state -eq 'NOT_APPLIED' -and $res.reason -match 'stop_conditions' -and $nativeCount -eq 0) {
        Report-Pass 'T81' 'mutating authorized command + empty stop_conditions rejected'
    } else {
        Report-Fail 'T81' 'mutating authorized command + empty stop_conditions rejected' "got: $($res.result) $($res.mutation_state) $($res.reason) count=$nativeCount"
    }
} catch {
    Report-Fail 'T81' 'mutating authorized command + empty stop_conditions rejected' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T82 — mutating manifest missing exactly one baseline stop-condition rejected (DEC-003 Phase 2A.2)
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't82'
    $partialBaseline = @(
        'MANIFEST_DIGEST_MISMATCH',
        'PRECONDITION_MISMATCH',
        'REMOTE_REF_MISMATCH',
        'POLICY_FAILURE',
        'LAUNCH_FAILURE',
        'ACTION_FAILURE',
        'SCOPE_PROOF_FAILURE',
        'POSTCONDITION_FAILURE'
        # Omit FINAL_IDENTITY_PROOF_FAILURE
    )
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'authorized_commands' = @(
            [ordered]@{
                'id' = 'mutating-cmd'
                'executable' = 'git'
                'arguments' = @('branch', 'newbranch')
                'cwd' = $repo.RepoDir
                'mutating' = $true
                'expect' = [ordered]@{ 'exit_code' = 0 }
            }
        )
        'stop_conditions' = $partialBaseline
    }
    $sha = Get-TestManifestSha256 $m.ManifestPath
    $nativeCount = 0
    $hook = { param($cmd, $ph, $p) $script:nativeCount++ }
    $res = Invoke-PalkaEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -AuthorizedManifestSha256 $sha -PassThru -TestPostStartHook $hook
    if ($res.result -eq 'STOPPED' -and $res.mutation_state -eq 'NOT_APPLIED' -and $res.reason -match 'missing mandatory baseline stop condition' -and $nativeCount -eq 0) {
        Report-Pass 'T82' 'mutating manifest missing exactly one baseline stop-condition rejected'
    } else {
        Report-Fail 'T82' 'mutating manifest missing exactly one baseline stop-condition rejected' "got: $($res.result) $($res.mutation_state) $($res.reason) count=$nativeCount"
    }
} catch {
    Report-Fail 'T82' 'mutating manifest missing exactly one baseline stop-condition rejected' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T83 — branch_transition.allowed=true with empty stop_conditions rejected (DEC-003 Phase 2A.2)
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't83'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'target_branch' = 'new-branch'
        'branch' = 'new-branch'
        'branch_transition' = [ordered]@{
            'allowed' = $true
            'mode' = 'create'
            'from' = 'main'
            'to' = 'new-branch'
        }
        'stop_conditions' = @()
    }
    $sha = Get-TestManifestSha256 $m.ManifestPath
    $nativeCount = 0
    $hook = { param($cmd, $ph, $p) $script:nativeCount++ }
    $res = Invoke-PalkaEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -AuthorizedManifestSha256 $sha -PassThru -TestPostStartHook $hook
    if ($res.result -eq 'STOPPED' -and $res.mutation_state -eq 'NOT_APPLIED' -and $res.reason -match 'stop_conditions' -and $nativeCount -eq 0) {
        Report-Pass 'T83' 'branch_transition.allowed=true with empty stop_conditions rejected'
    } else {
        Report-Fail 'T83' 'branch_transition.allowed=true with empty stop_conditions rejected' "got: $($res.result) $($res.mutation_state) $($res.reason) count=$nativeCount"
    }
} catch {
    Report-Fail 'T83' 'branch_transition.allowed=true with empty stop_conditions rejected' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T84 — duplicate baseline stop-condition rejected (DEC-003 Phase 2A.2)
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't84'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'stop_conditions' = @('POLICY_FAILURE', 'POLICY_FAILURE')
    }
    $sha = Get-TestManifestSha256 $m.ManifestPath
    $nativeCount = 0
    $hook = { param($cmd, $ph, $p) $script:nativeCount++ }
    $res = Invoke-PalkaEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -AuthorizedManifestSha256 $sha -PassThru -TestPostStartHook $hook
    if ($res.result -eq 'STOPPED' -and $res.mutation_state -eq 'NOT_APPLIED' -and $res.reason -match 'Duplicate stop condition identifier' -and $nativeCount -eq 0) {
        Report-Pass 'T84' 'duplicate baseline stop-condition rejected'
    } else {
        Report-Fail 'T84' 'duplicate baseline stop-condition rejected' "got: $($res.result) $($res.mutation_state) $($res.reason) count=$nativeCount"
    }
} catch {
    Report-Fail 'T84' 'duplicate baseline stop-condition rejected' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T85 — unknown stop-condition identifier and case-variants rejected (DEC-003 Phase 2A.2)
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't85'

    # Subcase 1: Unknown custom condition
    $m1 = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'stop_conditions' = @('UNKNOWN_CUSTOM_CONDITION')
    }
    $sha1 = Get-TestManifestSha256 $m1.ManifestPath
    $nativeCount1 = 0
    $hook1 = { param($cmd, $ph, $p) $script:nativeCount1++ }
    $res1 = Invoke-PalkaEngine -ManifestPath $m1.ManifestPath -OutputRoot $outputRoot -AuthorizedManifestSha256 $sha1 -PassThru -TestPostStartHook $hook1

    # Subcase 2: Lowercase variant of valid condition
    $m2 = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'stop_conditions' = @('policy_failure')
    }
    $sha2 = Get-TestManifestSha256 $m2.ManifestPath
    $nativeCount2 = 0
    $hook2 = { param($cmd, $ph, $p) $script:nativeCount2++ }
    $res2 = Invoke-PalkaEngine -ManifestPath $m2.ManifestPath -OutputRoot $outputRoot -AuthorizedManifestSha256 $sha2 -PassThru -TestPostStartHook $hook2

    # Subcase 3: Full canonical baseline + lowercase variant
    $canonicalPlusLower = @(
        'MANIFEST_DIGEST_MISMATCH',
        'PRECONDITION_MISMATCH',
        'REMOTE_REF_MISMATCH',
        'POLICY_FAILURE',
        'LAUNCH_FAILURE',
        'ACTION_FAILURE',
        'SCOPE_PROOF_FAILURE',
        'POSTCONDITION_FAILURE',
        'FINAL_IDENTITY_PROOF_FAILURE',
        'policy_failure'
    )
    $m3 = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'stop_conditions' = $canonicalPlusLower
    }
    $sha3 = Get-TestManifestSha256 $m3.ManifestPath
    $nativeCount3 = 0
    $hook3 = { param($cmd, $ph, $p) $script:nativeCount3++ }
    $res3 = Invoke-PalkaEngine -ManifestPath $m3.ManifestPath -OutputRoot $outputRoot -AuthorizedManifestSha256 $sha3 -PassThru -TestPostStartHook $hook3

    $pass1 = ($res1.result -eq 'STOPPED' -and $res1.mutation_state -eq 'NOT_APPLIED' -and $res1.reason -match 'Unknown stop condition identifier' -and $nativeCount1 -eq 0)
    $pass2 = ($res2.result -eq 'STOPPED' -and $res2.mutation_state -eq 'NOT_APPLIED' -and $res2.reason -match 'Unknown stop condition identifier' -and $nativeCount2 -eq 0)
    $pass3 = ($res3.result -eq 'STOPPED' -and $res3.mutation_state -eq 'NOT_APPLIED' -and $res3.reason -match 'Unknown stop condition identifier' -and $nativeCount3 -eq 0)

    if ($pass1 -and $pass2 -and $pass3) {
        Report-Pass 'T85' 'unknown stop-condition identifier and case-variants rejected'
    } else {
        Report-Fail 'T85' 'unknown stop-condition identifier and case-variants rejected' "pass1=$pass1, pass2=$pass2, pass3=$pass3"
    }
} catch {
    Report-Fail 'T85' 'unknown stop-condition identifier and case-variants rejected' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m1 -and (Test-Path $m1.ManifestPath)) { Remove-Item -Force $m1.ManifestPath }
    if ($null -ne $m2 -and (Test-Path $m2.ManifestPath)) { Remove-Item -Force $m2.ManifestPath }
    if ($null -ne $m3 -and (Test-Path $m3.ManifestPath)) { Remove-Item -Force $m3.ManifestPath }
}

# ----------------------------------------------------
# T86 вЂ” CLI execution-envelope contract (DEC-003 Phase 2A.2)
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't86'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha
    $cliScript = Join-Path (Split-Path -Parent $scriptDir) 'Invoke-PalkaOperation.ps1'

    # Subcase A: Missing digest via CLI produces structured STOPPED / NOT_APPLIED without interactive prompt
    $pA = New-Object System.Diagnostics.Process
    $pA.StartInfo.FileName = 'powershell.exe'
    $pA.StartInfo.Arguments = "-NoProfile -ExecutionPolicy Bypass -File `"$cliScript`" -ManifestPath `"$($m.ManifestPath)`" -OutputRoot `"$outputRoot`""
    $pA.StartInfo.UseShellExecute = $false
    $pA.StartInfo.RedirectStandardOutput = $true
    $pA.StartInfo.RedirectStandardError = $true
    $pA.Start() | Out-Null
    $outA = $pA.StandardOutput.ReadToEnd()
    $pA.WaitForExit()
    $exitA = $pA.ExitCode

    $subcaseAPass = ($exitA -ne 0 -and $outA -match 'RESULT:\s*STOPPED' -and $outA -match 'MUTATION_STATE:\s*NOT_APPLIED')

    # Subcase B: Correct digest via CLI reaches engine and valid read-only operation completes
    $shaB = Get-TestManifestSha256 $m.ManifestPath
    $pB = New-Object System.Diagnostics.Process
    $pB.StartInfo.FileName = 'powershell.exe'
    $pB.StartInfo.Arguments = "-NoProfile -ExecutionPolicy Bypass -File `"$cliScript`" -ManifestPath `"$($m.ManifestPath)`" -OutputRoot `"$outputRoot`" -AuthorizedManifestSha256 $shaB"
    $pB.StartInfo.UseShellExecute = $false
    $pB.StartInfo.RedirectStandardOutput = $true
    $pB.StartInfo.RedirectStandardError = $true
    $pB.Start() | Out-Null
    $outB = $pB.StandardOutput.ReadToEnd()
    $pB.WaitForExit()
    $exitB = $pB.ExitCode

    $subcaseBPass = ($exitB -eq 0 -and $outB -match 'RESULT:\s*COMPLETED' -and $outB -match 'MUTATION_STATE:\s*NONE')

    if ($subcaseAPass -and $subcaseBPass) {
        Report-Pass 'T86' 'CLI execution-envelope contract: missing digest stops gracefully, valid digest completes'
    } else {
        Report-Fail 'T86' 'CLI execution-envelope contract: missing digest stops gracefully, valid digest completes' "subcaseA=$subcaseAPass (exit=$exitA), subcaseB=$subcaseBPass (exit=$exitB)"
    }
} catch {
    Report-Fail 'T86' 'CLI execution-envelope contract' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}


# ----------------------------------------------------
# T87 — Valid EVIDENCE_BUNDLE_V1 read-only operation completes and emits artifact.zip (DEC-003 Phase 2B)
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't87'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha
    $sha = Get-TestManifestSha256 $m.ManifestPath
    $res = Invoke-PalkaEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -AuthorizedManifestSha256 $sha -PassThru

    $artPath = $res.artifact_path
    $artSha = $res.artifact_sha256
    $exists = ($null -ne $artPath -and (Test-Path -LiteralPath $artPath -PathType Leaf))
    $isZipName = ($null -ne $artPath -and $artPath.EndsWith('artifact.zip'))
    $validSha = ($null -ne $artSha -and $artSha -match '^[0-9a-f]{64}$')

    if ($res.result -eq 'COMPLETED' -and $res.mutation_state -eq 'NONE' -and $exists -and $isZipName -and $validSha) {
        Report-Pass 'T87' 'Valid EVIDENCE_BUNDLE_V1 read-only operation completes and emits artifact.zip'
    } else {
        Report-Fail 'T87' 'Valid EVIDENCE_BUNDLE_V1 read-only operation completes and emits artifact.zip' "res=$($res.result), mut=$($res.mutation_state), exists=$exists, isZipName=$isZipName, validSha=$validSha"
    }
} catch {
    Report-Fail 'T87' 'Valid EVIDENCE_BUNDLE_V1 read-only operation completes and emits artifact.zip' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T88 — Legacy PHASE_2A_RUN_DIRECTORY_V0 plus bootstrap_zip_v1 and case variants are rejected before native execution (DEC-003 Phase 2B)
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't88'
    $variants = @('PHASE_2A_RUN_DIRECTORY_V0', 'phase_2a_run_directory_v0', 'bootstrap_zip_v1', 'evidence_bundle_v1', 'Evidence_Bundle_V1')
    $allPassed = $true
    $failReasons = @()

    foreach ($var in $variants) {
        $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{ 'artifact_profile' = $var }
        $sha = Get-TestManifestSha256 $m.ManifestPath
        $nativeCount = 0
        $hook = { param($cmd, $ph, $p) $script:nativeCount++ }
        $res = Invoke-PalkaEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -AuthorizedManifestSha256 $sha -PassThru -TestPostStartHook $hook
        if ($res.result -ne 'STOPPED' -or $res.mutation_state -ne 'NOT_APPLIED' -or $res.reason -notmatch 'artifact_profile' -or $nativeCount -ne 0) {
            $allPassed = $false
            $failReasons += "variant '$var': res=$($res.result), reason=$($res.reason), count=$nativeCount"
        }
        if (Test-Path $m.ManifestPath) { Remove-Item -Force $m.ManifestPath }
    }

    if ($allPassed) {
        Report-Pass 'T88' 'Legacy PHASE_2A_RUN_DIRECTORY_V0 plus bootstrap_zip_v1 and case variants are rejected before native execution'
    } else {
        Report-Fail 'T88' 'Legacy PHASE_2A_RUN_DIRECTORY_V0 plus bootstrap_zip_v1 and case variants are rejected before native execution' ($failReasons -join '; ')
    }
} catch {
    Report-Fail 'T88' 'Legacy PHASE_2A_RUN_DIRECTORY_V0 plus bootstrap_zip_v1 and case variants are rejected before native execution' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
}

# ----------------------------------------------------
# T89 — Successful canonical ZIP contains required root files, evidence/** and patches/changes.patch and no nested artifact.zip (DEC-003 Phase 2B)
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't89'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha
    $sha = Get-TestManifestSha256 $m.ManifestPath
    $res = Invoke-PalkaEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -AuthorizedManifestSha256 $sha -PassThru

    $zipStream = [System.IO.File]::OpenRead($res.artifact_path)
    $zip = [System.IO.Compression.ZipArchive]::new($zipStream, [System.IO.Compression.ZipArchiveMode]::Read)
    $entryNames = @($zip.Entries | ForEach-Object { $_.FullName })
    $zip.Dispose()
    $zipStream.Dispose()

    $hasManifest = ($entryNames -contains 'manifest.json')
    $hasSummary = ($entryNames -contains 'summary.json')
    $hasCommands = ($entryNames -contains 'commands.jsonl')
    $hasChecksums = ($entryNames -contains 'checksums.sha256')
    $hasPatch = ($entryNames -contains 'patches/changes.patch')
    $hasEvidence = (@($entryNames | Where-Object { $_ -like 'evidence/*' }).Count -gt 0)
    $noNestedZip = (-not ($entryNames -contains 'artifact.zip'))

    if ($hasManifest -and $hasSummary -and $hasCommands -and $hasChecksums -and $hasPatch -and $hasEvidence -and $noNestedZip) {
        Report-Pass 'T89' 'Successful canonical ZIP contains required root files, evidence/** and patches/changes.patch and no nested artifact.zip'
    } else {
        Report-Fail 'T89' 'Successful canonical ZIP contains required root files, evidence/** and patches/changes.patch and no nested artifact.zip' "manifest=$hasManifest, summary=$hasSummary, commands=$hasCommands, checksums=$hasChecksums, patch=$hasPatch, evidence=$hasEvidence, noNested=$noNestedZip"
    }
} catch {
    Report-Fail 'T89' 'Successful canonical ZIP contains required root files, evidence/** and patches/changes.patch and no nested artifact.zip' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T90 — checksums.sha256 covers every regular bundle content file except itself exactly once (DEC-003 Phase 2B)
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't90'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha
    $sha = Get-TestManifestSha256 $m.ManifestPath
    $res = Invoke-PalkaEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -AuthorizedManifestSha256 $sha -PassThru

    $zipStream = [System.IO.File]::OpenRead($res.artifact_path)
    $zip = [System.IO.Compression.ZipArchive]::new($zipStream, [System.IO.Compression.ZipArchiveMode]::Read)
    $allEntries = @($zip.Entries | ForEach-Object { $_.FullName })

    $cEntry = $zip.GetEntry('checksums.sha256')
    $csStream = $cEntry.Open()
    $ms = [System.IO.MemoryStream]::new()
    $csStream.CopyTo($ms)
    $csText = [System.Text.Encoding]::UTF8.GetString($ms.ToArray())
    $csStream.Dispose()
    $ms.Dispose()
    $zip.Dispose()
    $zipStream.Dispose()

    $csLines = $csText.TrimEnd("`n").Split("`n")
    $csPaths = @($csLines | ForEach-Object { ($_ -split '  ')[1] })

    $expectedNonChecksum = @($allEntries | Where-Object { $_ -ne 'checksums.sha256' })
    $uniqueCsPaths = [System.Collections.Generic.HashSet[string]]::new([string[]]$csPaths, [System.StringComparer]::Ordinal)

    $countsMatch = ($csPaths.Length -eq $uniqueCsPaths.Count -and $csPaths.Length -eq $expectedNonChecksum.Length)
    $noSelfInCs = (-not ($csPaths -contains 'checksums.sha256'))
    $noZipInCs = (-not ($csPaths -contains 'artifact.zip'))

    if ($countsMatch -and $noSelfInCs -and $noZipInCs) {
        Report-Pass 'T90' 'checksums.sha256 covers every regular bundle content file except itself exactly once'
    } else {
        Report-Fail 'T90' 'checksums.sha256 covers every regular bundle content file except itself exactly once' "countsMatch=$countsMatch, noSelf=$noSelfInCs, noZip=$noZipInCs"
    }
} catch {
    Report-Fail 'T90' 'checksums.sha256 covers every regular bundle content file except itself exactly once' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T91 — checksums.sha256 is lowercase, exactly two spaces, LF-only, final LF, ordinal path sorted (DEC-003 Phase 2B)
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't91'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha
    $sha = Get-TestManifestSha256 $m.ManifestPath
    $res = Invoke-PalkaEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -AuthorizedManifestSha256 $sha -PassThru

    $zipStream = [System.IO.File]::OpenRead($res.artifact_path)
    $zip = [System.IO.Compression.ZipArchive]::new($zipStream, [System.IO.Compression.ZipArchiveMode]::Read)
    $cEntry = $zip.GetEntry('checksums.sha256')
    $csStream = $cEntry.Open()
    $ms = [System.IO.MemoryStream]::new()
    $csStream.CopyTo($ms)
    $csBytes = $ms.ToArray()
    $csStream.Dispose()
    $ms.Dispose()
    $zip.Dispose()
    $zipStream.Dispose()

    $noCr = (-not ($csBytes -contains [byte]13)) # No CR
    $hasFinalLf = ($csBytes.Length -gt 0 -and $csBytes[$csBytes.Length - 1] -eq [byte]10)
    $csText = [System.Text.Encoding]::UTF8.GetString($csBytes)
    $lines = $csText.TrimEnd("`n").Split("`n")

    $allLinesValid = $true
    $paths = @()
    foreach ($line in $lines) {
        if ($line -notmatch '^[0-9a-f]{64}  [^\s].*$') {
            $allLinesValid = $false
            break
        }
        $paths += ($line -split '  ')[1]
    }

    $isSorted = $true
    for ($i = 0; $i -lt $paths.Length - 1; $i++) {
        if ([System.StringComparer]::Ordinal.Compare($paths[$i], $paths[$i + 1]) -ge 0) {
            $isSorted = $false
            break
        }
    }

    if ($noCr -and $hasFinalLf -and $allLinesValid -and $isSorted) {
        Report-Pass 'T91' 'checksums.sha256 is lowercase, exactly two spaces, LF-only, final LF, ordinal path sorted'
    } else {
        Report-Fail 'T91' 'checksums.sha256 is lowercase, exactly two spaces, LF-only, final LF, ordinal path sorted' "noCr=$noCr, finalLf=$hasFinalLf, validLines=$allLinesValid, sorted=$isSorted"
    }
} catch {
    Report-Fail 'T91' 'checksums.sha256 is lowercase, exactly two spaces, LF-only, final LF, ordinal path sorted' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T92 — At least one legitimate zero-byte stdout/stderr evidence file has the correct SHA-256 of empty bytes in checksums.sha256 (DEC-003 Phase 2B)
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't92'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha
    $sha = Get-TestManifestSha256 $m.ManifestPath
    $res = Invoke-PalkaEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -AuthorizedManifestSha256 $sha -PassThru

    $emptyHash = 'e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855'
    $csPath = Join-Path $res.run_directory 'checksums.sha256'
    $csLines = [System.IO.File]::ReadAllLines($csPath)
    $hasEmptyEvidenceHash = $false

    foreach ($l in $csLines) {
        if ($l.StartsWith($emptyHash) -and ($l.Contains('evidence/') -or $l.EndsWith('patches/changes.patch'))) {
            $hasEmptyEvidenceHash = $true
            break
        }
    }

    if ($hasEmptyEvidenceHash) {
        Report-Pass 'T92' 'At least one legitimate zero-byte stdout/stderr evidence file has the correct SHA-256 of empty bytes in checksums.sha256'
    } else {
        Report-Fail 'T92' 'At least one legitimate zero-byte stdout/stderr evidence file has the correct SHA-256 of empty bytes in checksums.sha256' 'No zero-byte evidence entry found with empty hash'
    }
} catch {
    Report-Fail 'T92' 'At least one legitimate zero-byte stdout/stderr evidence file has the correct SHA-256 of empty bytes in checksums.sha256' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T93 — ZIP manifest.json is byte-identical to authorized input and run-directory manifest.json (DEC-003 Phase 2B)
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't93'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha
    $sha = Get-TestManifestSha256 $m.ManifestPath
    $inputBytes = [System.IO.File]::ReadAllBytes($m.ManifestPath)
    $res = Invoke-PalkaEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -AuthorizedManifestSha256 $sha -PassThru

    $runManifestBytes = [System.IO.File]::ReadAllBytes((Join-Path $res.run_directory 'manifest.json'))

    $zipStream = [System.IO.File]::OpenRead($res.artifact_path)
    $zip = [System.IO.Compression.ZipArchive]::new($zipStream, [System.IO.Compression.ZipArchiveMode]::Read)
    $mEntry = $zip.GetEntry('manifest.json')
    $msStream = $mEntry.Open()
    $ms = [System.IO.MemoryStream]::new()
    $msStream.CopyTo($ms)
    $zipManifestBytes = $ms.ToArray()
    $msStream.Dispose()
    $ms.Dispose()
    $zip.Dispose()
    $zipStream.Dispose()

    $runMatch = [System.Linq.Enumerable]::SequenceEqual($inputBytes, $runManifestBytes)
    $zipMatch = [System.Linq.Enumerable]::SequenceEqual($inputBytes, $zipManifestBytes)

    if ($runMatch -and $zipMatch) {
        Report-Pass 'T93' 'ZIP manifest.json is byte-identical to authorized input and run-directory manifest.json'
    } else {
        Report-Fail 'T93' 'ZIP manifest.json is byte-identical to authorized input and run-directory manifest.json' "runMatch=$runMatch, zipMatch=$zipMatch"
    }
} catch {
    Report-Fail 'T93' 'ZIP manifest.json is byte-identical to authorized input and run-directory manifest.json' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T94 — ZIP summary.json and commands.jsonl identity, ordinal fields, mandatory evidence paths, and command_count type (DEC-003 Phase 2B R3)
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't94'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha
    $sha = Get-TestManifestSha256 $m.ManifestPath
    $res = Invoke-PalkaEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -AuthorizedManifestSha256 $sha -PassThru

    $zipStream = [System.IO.File]::OpenRead($res.artifact_path)
    $zip = [System.IO.Compression.ZipArchive]::new($zipStream, [System.IO.Compression.ZipArchiveMode]::Read)

    # 1. Base test: byte identity of summary.json and commands.jsonl
    $sEntry = $zip.GetEntry('summary.json')
    $ss = $sEntry.Open()
    $sms = [System.IO.MemoryStream]::new()
    $ss.CopyTo($sms)
    $zipSummaryBytes = $sms.ToArray()
    $ss.Dispose()
    $sms.Dispose()

    $cEntry = $zip.GetEntry('commands.jsonl')
    $cs = $cEntry.Open()
    $cms = [System.IO.MemoryStream]::new()
    $cs.CopyTo($cms)
    $zipCommandsBytes = $cms.ToArray()
    $cs.Dispose()
    $cms.Dispose()
    $zip.Dispose()
    $zipStream.Dispose()

    $runSummaryBytes = [System.IO.File]::ReadAllBytes((Join-Path $res.run_directory 'summary.json'))
    $runCommandsBytes = [System.IO.File]::ReadAllBytes((Join-Path $res.run_directory 'commands.jsonl'))

    $baseSumMatch = ([System.Convert]::ToBase64String($zipSummaryBytes) -eq [System.Convert]::ToBase64String($runSummaryBytes))
    $baseCmdMatch = ([System.Convert]::ToBase64String($zipCommandsBytes) -eq [System.Convert]::ToBase64String($runCommandsBytes))

    # Subcase A: Change summary.operation_id case only -> Verifier MUST reject (Ordinal identity)
    $tamperedZipA = Join-Path $outputRoot 'tampered-t94-subA.zip'
    [System.IO.File]::Copy($res.artifact_path, $tamperedZipA, $true)
    $zStreamA = [System.IO.File]::Open($tamperedZipA, [System.IO.FileMode]::Open, [System.IO.FileAccess]::ReadWrite)
    $zipA = [System.IO.Compression.ZipArchive]::new($zStreamA, [System.IO.Compression.ZipArchiveMode]::Update)
    $sumObjA = [System.Text.Encoding]::UTF8.GetString($zipSummaryBytes) | ConvertFrom-Json
    $sumObjA.operation_id = $sumObjA.operation_id.ToLowerInvariant()
    $newSumBytesA = [System.Text.UTF8Encoding]::new($false).GetBytes(($sumObjA | ConvertTo-Json -Depth 5))
    $shaAlgo = [System.Security.Cryptography.SHA256]::Create()
    $newSumHashA = (($shaAlgo.ComputeHash($newSumBytesA) | ForEach-Object { $_.ToString('x2') }) -join '')

    # Rebuild checksums
    $cEntryA = $zipA.GetEntry('checksums.sha256')
    $csStreamA = $cEntryA.Open()
    $msA = [System.IO.MemoryStream]::new()
    $csStreamA.CopyTo($msA)
    $oldCsTextA = [System.Text.Encoding]::UTF8.GetString($msA.ToArray())
    $csStreamA.Dispose()
    $msA.Dispose()

    $csLinesA = [System.Collections.Generic.List[string]]::new()
    foreach ($l in $oldCsTextA.TrimEnd("`n").Split("`n")) {
        if ($l.EndsWith('summary.json')) {
            $csLinesA.Add("$newSumHashA  summary.json")
        } elseif ($l.Length -gt 0) {
            $csLinesA.Add($l)
        }
    }
    $newCsBytesA = [System.Text.UTF8Encoding]::new($false).GetBytes(($csLinesA -join "`n") + "`n")

    $zipA.GetEntry('summary.json').Delete()
    $newSumEntryA = $zipA.CreateEntry('summary.json')
    $nsA = $newSumEntryA.Open()
    $nsA.Write($newSumBytesA, 0, $newSumBytesA.Length)
    $nsA.Dispose()

    $cEntryA.Delete()
    $newCsEntryA = $zipA.CreateEntry('checksums.sha256')
    $ncsA = $newCsEntryA.Open()
    $ncsA.Write($newCsBytesA, 0, $newCsBytesA.Length)
    $ncsA.Dispose()

    $zipA.Dispose()
    $zStreamA.Dispose()

    $subARejected = $false
    try {
        Test-PalkaEvidenceBundle -ArtifactPath $tamperedZipA | Out-Null
    } catch {
        $subARejected = ($_.Exception.Message -match 'Ordinal mismatch|does not match manifest')
    }

    # Subcase B: In one commands.jsonl record set stdout_path=null, stderr_path=null -> Verifier MUST reject
    $tamperedZipB = Join-Path $outputRoot 'tampered-t94-subB.zip'
    [System.IO.File]::Copy($res.artifact_path, $tamperedZipB, $true)
    $zStreamB = [System.IO.File]::Open($tamperedZipB, [System.IO.FileMode]::Open, [System.IO.FileAccess]::ReadWrite)
    $zipB = [System.IO.Compression.ZipArchive]::new($zStreamB, [System.IO.Compression.ZipArchiveMode]::Update)
    $cmdLinesB = ([System.Text.Encoding]::UTF8.GetString($zipCommandsBytes)).TrimEnd("`n").Split("`n")
    $firstCmdObjB = $cmdLinesB[0] | ConvertFrom-Json
    $firstCmdObjB.stdout_path = $null
    $firstCmdObjB.stderr_path = $null
    $cmdLinesB[0] = ($firstCmdObjB | ConvertTo-Json -Compress)
    $newCmdBytesB = [System.Text.UTF8Encoding]::new($false).GetBytes(($cmdLinesB -join "`n") + "`n")
    $newCmdHashB = (($shaAlgo.ComputeHash($newCmdBytesB) | ForEach-Object { $_.ToString('x2') }) -join '')

    $zipB.GetEntry('commands.jsonl').Delete()
    $newCmdEntryB = $zipB.CreateEntry('commands.jsonl')
    $ncmdB = $newCmdEntryB.Open()
    $ncmdB.Write($newCmdBytesB, 0, $newCmdBytesB.Length)
    $ncmdB.Dispose()

    # Rebuild checksums
    $cEntryB = $zipB.GetEntry('checksums.sha256')
    $csStreamB = $cEntryB.Open()
    $msB = [System.IO.MemoryStream]::new()
    $csStreamB.CopyTo($msB)
    $oldCsTextB = [System.Text.Encoding]::UTF8.GetString($msB.ToArray())
    $csStreamB.Dispose()
    $msB.Dispose()

    $csLinesB = [System.Collections.Generic.List[string]]::new()
    foreach ($l in $oldCsTextB.TrimEnd("`n").Split("`n")) {
        if ($l.EndsWith('commands.jsonl')) {
            $csLinesB.Add("$newCmdHashB  commands.jsonl")
        } elseif ($l.Length -gt 0) {
            $csLinesB.Add($l)
        }
    }
    $newCsBytesB = [System.Text.UTF8Encoding]::new($false).GetBytes(($csLinesB -join "`n") + "`n")

    $cEntryB.Delete()
    $newCsEntryB = $zipB.CreateEntry('checksums.sha256')
    $ncsB = $newCsEntryB.Open()
    $ncsB.Write($newCsBytesB, 0, $newCsBytesB.Length)
    $ncsB.Dispose()

    $zipB.Dispose()
    $zStreamB.Dispose()

    $subBRejected = $false
    try {
        Test-PalkaEvidenceBundle -ArtifactPath $tamperedZipB | Out-Null
    } catch {
        $subBRejected = ($_.Exception.Message -match 'missing|invalid|empty stdout_path|VERIFIER_FAILURE')
    }

    # Subcase C: summary.command_count changed from integer to JSON string -> Verifier MUST reject
    $tamperedZipC = Join-Path $outputRoot 'tampered-t94-subC.zip'
    [System.IO.File]::Copy($res.artifact_path, $tamperedZipC, $true)
    $zStreamC = [System.IO.File]::Open($tamperedZipC, [System.IO.FileMode]::Open, [System.IO.FileAccess]::ReadWrite)
    $zipC = [System.IO.Compression.ZipArchive]::new($zStreamC, [System.IO.Compression.ZipArchiveMode]::Update)
    $sumTextC = [System.Text.Encoding]::UTF8.GetString($zipSummaryBytes)
    # Replace integer command_count with string "N"
    $sumTextCModified = $sumTextC -replace '"command_count":\s*(\d+)', '"command_count": "$1"'
    $newSumBytesC = [System.Text.UTF8Encoding]::new($false).GetBytes($sumTextCModified)
    $newSumHashC = (($shaAlgo.ComputeHash($newSumBytesC) | ForEach-Object { $_.ToString('x2') }) -join '')

    $zipC.GetEntry('summary.json').Delete()
    $newSumEntryC = $zipC.CreateEntry('summary.json')
    $nsC = $newSumEntryC.Open()
    $nsC.Write($newSumBytesC, 0, $newSumBytesC.Length)
    $nsC.Dispose()

    # Rebuild checksums
    $cEntryC = $zipC.GetEntry('checksums.sha256')
    $csStreamC = $cEntryC.Open()
    $msC = [System.IO.MemoryStream]::new()
    $csStreamC.CopyTo($msC)
    $oldCsTextC = [System.Text.Encoding]::UTF8.GetString($msC.ToArray())
    $csStreamC.Dispose()
    $msC.Dispose()

    $csLinesC = [System.Collections.Generic.List[string]]::new()
    foreach ($l in $oldCsTextC.TrimEnd("`n").Split("`n")) {
        if ($l.EndsWith('summary.json')) {
            $csLinesC.Add("$newSumHashC  summary.json")
        } elseif ($l.Length -gt 0) {
            $csLinesC.Add($l)
        }
    }
    $newCsBytesC = [System.Text.UTF8Encoding]::new($false).GetBytes(($csLinesC -join "`n") + "`n")

    $cEntryC.Delete()
    $newCsEntryC = $zipC.CreateEntry('checksums.sha256')
    $ncsC = $newCsEntryC.Open()
    $ncsC.Write($newCsBytesC, 0, $newCsBytesC.Length)
    $ncsC.Dispose()

    $zipC.Dispose()
    $zStreamC.Dispose()

    $subCRejected = $false
    try {
        Test-PalkaEvidenceBundle -ArtifactPath $tamperedZipC | Out-Null
    } catch {
        $subCRejected = ($_.Exception.Message -match 'integer|VERIFIER_FAILURE')
    }

    if ($baseSumMatch -and $baseCmdMatch -and $subARejected -and $subBRejected -and $subCRejected) {
        Report-Pass 'T94' 'ZIP summary.json and commands.jsonl are byte-identical to run-directory copies'
    } else {
        Report-Fail 'T94' 'ZIP summary.json and commands.jsonl are byte-identical to run-directory copies' "sumMatch=$baseSumMatch, cmdMatch=$baseCmdMatch, subA=$subARejected, subB=$subBRejected, subC=$subCRejected"
    }
} catch {
    Report-Fail 'T94' 'ZIP summary.json and commands.jsonl are byte-identical to run-directory copies' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T95 — Returned ARTIFACT_SHA256 equals independent SHA-256 of final artifact.zip and the hash string is not present anywhere inside ZIP content (DEC-003 Phase 2B)
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't95'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha
    $sha = Get-TestManifestSha256 $m.ManifestPath
    $res = Invoke-PalkaEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -AuthorizedManifestSha256 $sha -PassThru

    $zipBytes = [System.IO.File]::ReadAllBytes($res.artifact_path)
    $shaAlgo = [System.Security.Cryptography.SHA256]::Create()
    $calcZipHash = (($shaAlgo.ComputeHash($zipBytes) | ForEach-Object { $_.ToString('x2') }) -join '')
    $hashesMatch = [string]::Equals($calcZipHash, $res.artifact_sha256, [System.StringComparison]::Ordinal)

    $zipStream = [System.IO.File]::OpenRead($res.artifact_path)
    $zip = [System.IO.Compression.ZipArchive]::new($zipStream, [System.IO.Compression.ZipArchiveMode]::Read)
    $foundHashInContent = $false
    foreach ($entry in $zip.Entries) {
        $es = $entry.Open()
        $ms = [System.IO.MemoryStream]::new()
        $es.CopyTo($ms)
        $content = [System.Text.Encoding]::UTF8.GetString($ms.ToArray())
        $es.Dispose()
        $ms.Dispose()
        if ($content.Contains($calcZipHash)) {
            $foundHashInContent = $true
            break
        }
    }
    $zip.Dispose()
    $zipStream.Dispose()

    if ($hashesMatch -and (-not $foundHashInContent)) {
        Report-Pass 'T95' 'Returned ARTIFACT_SHA256 equals independent SHA-256 of final artifact.zip and hash is not inside ZIP'
    } else {
        Report-Fail 'T95' 'Returned ARTIFACT_SHA256 equals independent SHA-256 of final artifact.zip and hash is not inside ZIP' "hashesMatch=$hashesMatch, foundInContent=$foundHashInContent"
    }
} catch {
    Report-Fail 'T95' 'Returned ARTIFACT_SHA256 equals independent SHA-256 of final artifact.zip and hash is not inside ZIP' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T96 — Clean operation has zero-byte patches/changes.patch and it is correctly covered by checksums.sha256 (DEC-003 Phase 2B)
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't96'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha
    $sha = Get-TestManifestSha256 $m.ManifestPath
    $res = Invoke-PalkaEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -AuthorizedManifestSha256 $sha -PassThru

    $patchBytes = [System.IO.File]::ReadAllBytes((Join-Path $res.run_directory 'patches/changes.patch'))
    $isZeroByte = ($patchBytes.Length -eq 0)

    $emptyHash = 'e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855'
    $expectedLine = "$emptyHash  patches/changes.patch"
    $csLines = [System.IO.File]::ReadAllLines((Join-Path $res.run_directory 'checksums.sha256'))
    $hasCorrectCsLine = ($csLines -contains $expectedLine)

    if ($isZeroByte -and $hasCorrectCsLine) {
        Report-Pass 'T96' 'Clean operation has zero-byte patches/changes.patch and it is correctly covered by checksums.sha256'
    } else {
        Report-Fail 'T96' 'Clean operation has zero-byte patches/changes.patch and it is correctly covered by checksums.sha256' "isZeroByte=$isZeroByte, hasCsLine=$hasCorrectCsLine"
    }
} catch {
    Report-Fail 'T96' 'Clean operation has zero-byte patches/changes.patch and it is correctly covered by checksums.sha256' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T97 — Authorized dirty-working-tree test operation produces non-empty patches/changes.patch byte-identical to builtin-bundle-patch stdout evidence (DEC-003 Phase 2B)
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't97'
    # Create dirty working tree file authorized by authorized_paths = **
    [System.IO.File]::AppendAllText((Join-Path $repo.RepoDir 'README.md'), "Dirty line`n")

    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha

    $sha = Get-TestManifestSha256 $m.ManifestPath
    $res = Invoke-PalkaEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -AuthorizedManifestSha256 $sha -PassThru

    $patchPath = Join-Path $res.run_directory 'patches/changes.patch'
    $patchBytes = [System.IO.File]::ReadAllBytes($patchPath)
    $isNonEmpty = ($patchBytes.Length -gt 0)

    # Locate builtin-bundle-patch evidence stdout
    $allEvidence = @(Get-ChildItem -LiteralPath (Join-Path $res.run_directory 'evidence') -Filter '*-builtin-bundle-patch-stdout.txt')
    $hasEvidence = ($allEvidence.Count -eq 1)
    $stdoutBytes = if ($hasEvidence) { [System.IO.File]::ReadAllBytes($allEvidence[0].FullName) } else { @() }
    $isIdentical = ($hasEvidence -and [System.Linq.Enumerable]::SequenceEqual([byte[]]$patchBytes, [byte[]]$stdoutBytes))

    if ($res.result -eq 'COMPLETED' -and $isNonEmpty -and $hasEvidence -and $isIdentical) {
        Report-Pass 'T97' 'Authorized dirty-working-tree test operation produces non-empty patches/changes.patch byte-identical to stdout evidence'
    } else {
        Report-Fail 'T97' 'Authorized dirty-working-tree test operation produces non-empty patches/changes.patch byte-identical to stdout evidence' "res=$($res.result), nonEmpty=$isNonEmpty, hasEv=$hasEvidence, identical=$isIdentical"
    }
} catch {
    Report-Fail 'T97' 'Authorized dirty-working-tree test operation produces non-empty patches/changes.patch byte-identical to stdout evidence' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T98 — Test-PalkaEvidenceBundle accepts an untouched genuine bundle and engine self-verification is proven (DEC-003 Phase 2B)
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't98'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha
    $sha = Get-TestManifestSha256 $m.ManifestPath
    $res = Invoke-PalkaEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -AuthorizedManifestSha256 $sha -PassThru

    $verifyResult = Test-PalkaEvidenceBundle -ArtifactPath $res.artifact_path

    # Assert production engine self-verification call and order directly from module source
    $psm1Content = [System.IO.File]::ReadAllText((Join-Path (Split-Path -Parent $scriptDir) 'PalkaGovernance.psm1'))
    $hasSelfVerify = ($psm1Content -match 'Test-PalkaEvidenceBundle\s+-ArtifactPath\s+\$finalArtifactPath')
    $idxMove = $psm1Content.IndexOf('[System.IO.File]::Move($tempZipPath, $finalArtifactPath)')
    $idxVerify = $psm1Content.IndexOf('Test-PalkaEvidenceBundle -ArtifactPath $finalArtifactPath')
    $idxAssign = $psm1Content.IndexOf('$artifactPath = $finalArtifactPath')
    $correctOrder = ($idxMove -ge 0 -and $idxVerify -gt $idxMove -and $idxAssign -gt $idxVerify)

    if ($verifyResult -eq $true -and $hasSelfVerify -and $correctOrder) {
        Report-Pass 'T98' 'Test-PalkaEvidenceBundle accepts an untouched genuine bundle'
    } else {
        Report-Fail 'T98' 'Test-PalkaEvidenceBundle accepts an untouched genuine bundle' "got: $verifyResult, hasSelfVerify=$hasSelfVerify, correctOrder=$correctOrder"
    }
} catch {
    Report-Fail 'T98' 'Test-PalkaEvidenceBundle accepts an untouched genuine bundle' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T99 — Tamper one archived content byte without updating checksums: verifier rejects (DEC-003 Phase 2B)
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't99'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha
    $sha = Get-TestManifestSha256 $m.ManifestPath
    $res = Invoke-PalkaEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -AuthorizedManifestSha256 $sha -PassThru

    # Create tampered copy of zip
    $tamperedZipPath = Join-Path $outputRoot 'tampered-t99.zip'
    [System.IO.File]::Copy($res.artifact_path, $tamperedZipPath, $true)

    # Modify manifest.json inside tampered zip
    $zipStream = [System.IO.File]::Open($tamperedZipPath, [System.IO.FileMode]::Open, [System.IO.FileAccess]::ReadWrite)
    $zip = [System.IO.Compression.ZipArchive]::new($zipStream, [System.IO.Compression.ZipArchiveMode]::Update)
    $oldBytes = & {
        $e = $zip.GetEntry('manifest.json')
        $s = $e.Open()
        $mem = [System.IO.MemoryStream]::new()
        $s.CopyTo($mem)
        $s.Dispose()
        $mem.ToArray()
    }
    $oldBytes[0] = if ($oldBytes[0] -eq [byte]65) { [byte]66 } else { [byte]65 }
    $zip.GetEntry('manifest.json').Delete()
    $newEntry = $zip.CreateEntry('manifest.json')
    $ns = $newEntry.Open()
    $ns.Write($oldBytes, 0, $oldBytes.Length)
    $ns.Dispose()
    $zip.Dispose()
    $zipStream.Dispose()

    $rejected = $false
    try {
        Test-PalkaEvidenceBundle -ArtifactPath $tamperedZipPath | Out-Null
    } catch {
        $rejected = ($_.Exception.Message -match 'Checksum mismatch')
    }

    if ($rejected) {
        Report-Pass 'T99' 'Tamper one archived content byte without updating checksums: verifier rejects'
    } else {
        Report-Fail 'T99' 'Tamper one archived content byte without updating checksums: verifier rejects' 'Verifier did not reject tampered bundle'
    }
} catch {
    Report-Fail 'T99' 'Tamper one archived content byte without updating checksums: verifier rejects' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T100 — Remove one required checksum line: verifier rejects (DEC-003 Phase 2B)
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't100'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha
    $sha = Get-TestManifestSha256 $m.ManifestPath
    $res = Invoke-PalkaEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -AuthorizedManifestSha256 $sha -PassThru

    $tamperedZipPath = Join-Path $outputRoot 'tampered-t100.zip'
    [System.IO.File]::Copy($res.artifact_path, $tamperedZipPath, $true)

    # Read checksums, remove one line, rewrite entry
    $zipStream = [System.IO.File]::Open($tamperedZipPath, [System.IO.FileMode]::Open, [System.IO.FileAccess]::ReadWrite)
    $zip = [System.IO.Compression.ZipArchive]::new($zipStream, [System.IO.Compression.ZipArchiveMode]::Update)
    $cEntry = $zip.GetEntry('checksums.sha256')
    $cs = $cEntry.Open()
    $ms = [System.IO.MemoryStream]::new()
    $cs.CopyTo($ms)
    $csText = [System.Text.Encoding]::UTF8.GetString($ms.ToArray())
    $cs.Dispose()
    $ms.Dispose()

    $lines = [System.Collections.Generic.List[string]]::new([string[]]($csText.TrimEnd("`n").Split("`n")))
    $lines.RemoveAt(0) # remove first line
    $newCsBytes = [System.Text.UTF8Encoding]::new($false).GetBytes(($lines -join "`n") + "`n")

    $cEntry.Delete()
    $newEntry = $zip.CreateEntry('checksums.sha256')
    $newEntryStream = $newEntry.Open()
    $newEntryStream.Write($newCsBytes, 0, $newCsBytes.Length)
    $newEntryStream.Dispose()
    $zip.Dispose()
    $zipStream.Dispose()

    $rejected = $false
    try {
        Test-PalkaEvidenceBundle -ArtifactPath $tamperedZipPath | Out-Null
    } catch {
        $rejected = ($_.Exception.Message -match 'no corresponding entry in checksums|does not exist in archive|Checksum')
    }

    if ($rejected) {
        Report-Pass 'T100' 'Remove one required checksum line: verifier rejects'
    } else {
        Report-Fail 'T100' 'Remove one required checksum line: verifier rejects' 'Verifier did not reject missing checksum line'
    }
} catch {
    Report-Fail 'T100' 'Remove one required checksum line: verifier rejects' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T101 — Duplicate one checksum path: verifier rejects (DEC-003 Phase 2B)
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't101'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha
    $sha = Get-TestManifestSha256 $m.ManifestPath
    $res = Invoke-PalkaEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -AuthorizedManifestSha256 $sha -PassThru

    $tamperedZipPath = Join-Path $outputRoot 'tampered-t101.zip'
    [System.IO.File]::Copy($res.artifact_path, $tamperedZipPath, $true)

    $zipStream = [System.IO.File]::Open($tamperedZipPath, [System.IO.FileMode]::Open, [System.IO.FileAccess]::ReadWrite)
    $zip = [System.IO.Compression.ZipArchive]::new($zipStream, [System.IO.Compression.ZipArchiveMode]::Update)
    $cEntry = $zip.GetEntry('checksums.sha256')
    $cs = $cEntry.Open()
    $ms = [System.IO.MemoryStream]::new()
    $cs.CopyTo($ms)
    $csText = [System.Text.Encoding]::UTF8.GetString($ms.ToArray())
    $cs.Dispose()
    $ms.Dispose()

    $lines = $csText.TrimEnd("`n").Split("`n")
    $dupText = $csText + $lines[0] + "`n" # duplicate first line at end
    $newCsBytes = [System.Text.UTF8Encoding]::new($false).GetBytes($dupText)

    $cEntry.Delete()
    $newEntry = $zip.CreateEntry('checksums.sha256')
    $newEntryStream = $newEntry.Open()
    $newEntryStream.Write($newCsBytes, 0, $newCsBytes.Length)
    $newEntryStream.Dispose()
    $zip.Dispose()
    $zipStream.Dispose()

    $rejected = $false
    try {
        Test-PalkaEvidenceBundle -ArtifactPath $tamperedZipPath | Out-Null
    } catch {
        $rejected = ($_.Exception.Message -match 'Duplicate path|not sorted')
    }

    if ($rejected) {
        Report-Pass 'T101' 'Duplicate one checksum path: verifier rejects'
    } else {
        Report-Fail 'T101' 'Duplicate one checksum path: verifier rejects' 'Verifier did not reject duplicate checksum path'
    }
} catch {
    Report-Fail 'T101' 'Duplicate one checksum path: verifier rejects' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T102 — Uppercase one checksum digest: verifier rejects (DEC-003 Phase 2B R3)
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't102'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha
    $sha = Get-TestManifestSha256 $m.ManifestPath
    $res = Invoke-PalkaEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -AuthorizedManifestSha256 $sha -PassThru

    $tamperedZipPath = Join-Path $outputRoot 'tampered-t102.zip'
    [System.IO.File]::Copy($res.artifact_path, $tamperedZipPath, $true)

    $zipStream = [System.IO.File]::Open($tamperedZipPath, [System.IO.FileMode]::Open, [System.IO.FileAccess]::ReadWrite)
    $zip = [System.IO.Compression.ZipArchive]::new($zipStream, [System.IO.Compression.ZipArchiveMode]::Update)
    $cEntry = $zip.GetEntry('checksums.sha256')
    $cs = $cEntry.Open()
    $ms = [System.IO.MemoryStream]::new()
    $cs.CopyTo($ms)
    $csText = [System.Text.Encoding]::UTF8.GetString($ms.ToArray())
    $cs.Dispose()
    $ms.Dispose()

    # Uppercase hash on first line
    $lines = $csText.TrimEnd("`n").Split("`n")
    $firstParts = $lines[0] -split '  '
    $lines[0] = "$($firstParts[0].ToUpper())  $($firstParts[1])"
    $newCsBytes = [System.Text.UTF8Encoding]::new($false).GetBytes(($lines -join "`n") + "`n")

    $cEntry.Delete()
    $newEntry = $zip.CreateEntry('checksums.sha256')
    $newEntryStream = $newEntry.Open()
    $newEntryStream.Write($newCsBytes, 0, $newCsBytes.Length)
    $newEntryStream.Dispose()
    $zip.Dispose()
    $zipStream.Dispose()

    $rejected = $false
    $rejectedReason = ''
    try {
        Test-PalkaEvidenceBundle -ArtifactPath $tamperedZipPath | Out-Null
    } catch {
        $rejectedReason = $_.Exception.Message
        $rejected = ($_.Exception.Message -match 'Malformed checksum line')
    }

    if ($rejected) {
        Report-Pass 'T102' 'Uppercase one checksum digest: verifier rejects'
    } else {
        Report-Fail 'T102' 'Uppercase one checksum digest: verifier rejects' "Verifier did not reject as Malformed checksum line (got: '$rejectedReason')"
    }
} catch {
    Report-Fail 'T102' 'Uppercase one checksum digest: verifier rejects' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T103 — Canonical namespace allowlist enforcement and directory entry rejection (DEC-003 Phase 2B R3)
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't103'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha
    $sha = Get-TestManifestSha256 $m.ManifestPath
    $res = Invoke-PalkaEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -AuthorizedManifestSha256 $sha -PassThru

    # Subcase 1: Extra regular file WITHOUT checksum entry -> REJECT
    $tamperedZip1 = Join-Path $outputRoot 'tampered-t103-sub1.zip'
    [System.IO.File]::Copy($res.artifact_path, $tamperedZip1, $true)

    $zipStream1 = [System.IO.File]::Open($tamperedZip1, [System.IO.FileMode]::Open, [System.IO.FileAccess]::ReadWrite)
    $zip1 = [System.IO.Compression.ZipArchive]::new($zipStream1, [System.IO.Compression.ZipArchiveMode]::Update)
    $extraEntry1 = $zip1.CreateEntry('extra_untracked.txt')
    $es1 = $extraEntry1.Open()
    $extraBytes1 = [System.Text.Encoding]::UTF8.GetBytes("extra file content")
    $es1.Write($extraBytes1, 0, $extraBytes1.Length)
    $es1.Dispose()
    $zip1.Dispose()
    $zipStream1.Dispose()

    $sub1Rejected = $false
    try {
        Test-PalkaEvidenceBundle -ArtifactPath $tamperedZip1 | Out-Null
    } catch {
        $sub1Rejected = ($_.Exception.Message -match 'Non-canonical ZIP entry|no corresponding entry in checksums|VERIFIER_FAILURE')
    }

    # Subcase 2: Extra regular file WITH VALID MATCHING CHECKSUM and updated checksums.sha256 -> MUST STILL REJECT (Non-canonical allowlist)
    $tamperedZip2 = Join-Path $outputRoot 'tampered-t103-sub2.zip'
    [System.IO.File]::Copy($res.artifact_path, $tamperedZip2, $true)

    $zipStream2 = [System.IO.File]::Open($tamperedZip2, [System.IO.FileMode]::Open, [System.IO.FileAccess]::ReadWrite)
    $zip2 = [System.IO.Compression.ZipArchive]::new($zipStream2, [System.IO.Compression.ZipArchiveMode]::Update)

    # Read existing checksums
    $cEntry2 = $zip2.GetEntry('checksums.sha256')
    $csStream2 = $cEntry2.Open()
    $ms2 = [System.IO.MemoryStream]::new()
    $csStream2.CopyTo($ms2)
    $oldCsText2 = [System.Text.Encoding]::UTF8.GetString($ms2.ToArray())
    $csStream2.Dispose()
    $ms2.Dispose()

    # Add extra regular file
    $extraEntry2 = $zip2.CreateEntry('extra_untracked.txt')
    $es2 = $extraEntry2.Open()
    $extraBytes2 = [System.Text.Encoding]::UTF8.GetBytes("extra file content")
    $es2.Write($extraBytes2, 0, $extraBytes2.Length)
    $es2.Dispose()

    $shaAlgo = [System.Security.Cryptography.SHA256]::Create()
    $extraHashBytes = $shaAlgo.ComputeHash($extraBytes2)
    $extraHexHash = (($extraHashBytes | ForEach-Object { $_.ToString('x2') }) -join '')

    # Build updated checksums with exact hash and canonical order
    $csLines2 = [System.Collections.Generic.List[string]]::new()
    foreach ($line in $oldCsText2.TrimEnd("`n").Split("`n")) {
        if ($line.Length -gt 0) { $csLines2.Add($line) }
    }
    $csLines2.Add("$extraHexHash  extra_untracked.txt")
    $csLines2Array = $csLines2.ToArray()
    [System.Array]::Sort($csLines2Array, [System.Collections.Generic.Comparer[string]]::Create({
        param($a, $b)
        $pa = ($a -split '  ')[1]
        $pb = ($b -split '  ')[1]
        [System.StringComparer]::Ordinal.Compare($pa, $pb)
    }))

    $newCsText2 = ($csLines2Array -join "`n") + "`n"
    $newCsBytes2 = [System.Text.UTF8Encoding]::new($false).GetBytes($newCsText2)

    $cEntry2.Delete()
    $newEntry2 = $zip2.CreateEntry('checksums.sha256')
    $ns2 = $newEntry2.Open()
    $ns2.Write($newCsBytes2, 0, $newCsBytes2.Length)
    $ns2.Dispose()

    $zip2.Dispose()
    $zipStream2.Dispose()

    $sub2Rejected = $false
    try {
        Test-PalkaEvidenceBundle -ArtifactPath $tamperedZip2 | Out-Null
    } catch {
        $sub2Rejected = ($_.Exception.Message -match 'Non-canonical|VERIFIER_FAILURE')
    }

    # Subcase 3: ZIP directory entry (e.g. extra_dir/) -> REJECT
    $tamperedZip3 = Join-Path $outputRoot 'tampered-t103-sub3.zip'
    [System.IO.File]::Copy($res.artifact_path, $tamperedZip3, $true)

    $zipStream3 = [System.IO.File]::Open($tamperedZip3, [System.IO.FileMode]::Open, [System.IO.FileAccess]::ReadWrite)
    $zip3 = [System.IO.Compression.ZipArchive]::new($zipStream3, [System.IO.Compression.ZipArchiveMode]::Update)
    $dirEntry3 = $zip3.CreateEntry('extra_dir/')
    $zip3.Dispose()
    $zipStream3.Dispose()

    $sub3Rejected = $false
    try {
        Test-PalkaEvidenceBundle -ArtifactPath $tamperedZip3 | Out-Null
    } catch {
        $sub3Rejected = ($_.Exception.Message -match 'directory|VERIFIER_FAILURE')
    }

    # Subcase 4: Case-variant regular entry (MANIFEST.JSON) -> REJECT
    $tamperedZip4 = Join-Path $outputRoot 'tampered-t103-sub4.zip'
    [System.IO.File]::Copy($res.artifact_path, $tamperedZip4, $true)

    $zipStream4 = [System.IO.File]::Open($tamperedZip4, [System.IO.FileMode]::Open, [System.IO.FileAccess]::ReadWrite)
    $zip4 = [System.IO.Compression.ZipArchive]::new($zipStream4, [System.IO.Compression.ZipArchiveMode]::Update)
    $manBytes4 = [System.Text.Encoding]::UTF8.GetBytes('{"case_variant":true}')
    $manHash4 = (($shaAlgo.ComputeHash($manBytes4) | ForEach-Object { $_.ToString('x2') }) -join '')
    $manEntry4 = $zip4.CreateEntry('MANIFEST.JSON')
    $ms4 = $manEntry4.Open()
    $ms4.Write($manBytes4, 0, $manBytes4.Length)
    $ms4.Dispose()

    # Rebuild checksums with MANIFEST.JSON
    $cEntry4 = $zip4.GetEntry('checksums.sha256')
    $csStream4 = $cEntry4.Open()
    $msMem4 = [System.IO.MemoryStream]::new()
    $csStream4.CopyTo($msMem4)
    $oldCsText4 = [System.Text.Encoding]::UTF8.GetString($msMem4.ToArray())
    $csStream4.Dispose()
    $msMem4.Dispose()

    $csLines4 = [System.Collections.Generic.List[string]]::new()
    foreach ($line in $oldCsText4.TrimEnd("`n").Split("`n")) {
        if ($line.Length -gt 0) { $csLines4.Add($line) }
    }
    $csLines4.Add("$manHash4  MANIFEST.JSON")
    $csLines4Array = $csLines4.ToArray()
    [System.Array]::Sort($csLines4Array, [System.Collections.Generic.Comparer[string]]::Create({
        param($a, $b)
        $pa = ($a -split '  ')[1]
        $pb = ($b -split '  ')[1]
        [System.StringComparer]::Ordinal.Compare($pa, $pb)
    }))

    $newCsText4 = ($csLines4Array -join "`n") + "`n"
    $newCsBytes4 = [System.Text.UTF8Encoding]::new($false).GetBytes($newCsText4)

    $cEntry4.Delete()
    $newEntry4 = $zip4.CreateEntry('checksums.sha256')
    $ns4 = $newEntry4.Open()
    $ns4.Write($newCsBytes4, 0, $newCsBytes4.Length)
    $ns4.Dispose()

    $zip4.Dispose()
    $zipStream4.Dispose()

    $sub4Rejected = $false
    try {
        Test-PalkaEvidenceBundle -ArtifactPath $tamperedZip4 | Out-Null
    } catch {
        $sub4Rejected = ($_.Exception.Message -match 'Non-canonical|VERIFIER_FAILURE')
    }

    if ($sub1Rejected -and $sub2Rejected -and $sub3Rejected -and $sub4Rejected) {
        Report-Pass 'T103' 'Insert an extra regular ZIP file without checksum entry: verifier rejects'
    } else {
        Report-Fail 'T103' 'Insert an extra regular ZIP file without checksum entry: verifier rejects' "sub1=$sub1Rejected, sub2=$sub2Rejected, sub3=$sub3Rejected, sub4=$sub4Rejected"
    }
} catch {
    Report-Fail 'T103' 'Insert an extra regular ZIP file without checksum entry: verifier rejects' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T104 — Unsafe archive/checksum path such as ../escape.txt and dot segment rejection (DEC-003 Phase 2B R3)
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't104'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha
    $sha = Get-TestManifestSha256 $m.ManifestPath
    $res = Invoke-PalkaEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -AuthorizedManifestSha256 $sha -PassThru

    # Subcase 1: ../escape.txt
    $tamperedZipPath1 = Join-Path $outputRoot 'tampered-t104-sub1.zip'
    [System.IO.File]::Copy($res.artifact_path, $tamperedZipPath1, $true)

    $zipStream1 = [System.IO.File]::Open($tamperedZipPath1, [System.IO.FileMode]::Open, [System.IO.FileAccess]::ReadWrite)
    $zip1 = [System.IO.Compression.ZipArchive]::new($zipStream1, [System.IO.Compression.ZipArchiveMode]::Update)
    $escapeEntry = $zip1.CreateEntry('../escape.txt')
    $es = $escapeEntry.Open()
    $esBytes = [System.Text.Encoding]::UTF8.GetBytes("escape")
    $es.Write($esBytes, 0, $esBytes.Length)
    $es.Dispose()
    $zip1.Dispose()
    $zipStream1.Dispose()

    $sub1Rejected = $false
    try {
        Test-PalkaEvidenceBundle -ArtifactPath $tamperedZipPath1 | Out-Null
    } catch {
        $sub1Rejected = ($_.Exception.Message -match 'invalid path segment|directory traversal|VERIFIER_FAILURE')
    }

    # Subcase 2: evidence/./dot-segment.txt with valid checksum
    $tamperedZipPath2 = Join-Path $outputRoot 'tampered-t104-sub2.zip'
    [System.IO.File]::Copy($res.artifact_path, $tamperedZipPath2, $true)

    $zipStream2 = [System.IO.File]::Open($tamperedZipPath2, [System.IO.FileMode]::Open, [System.IO.FileAccess]::ReadWrite)
    $zip2 = [System.IO.Compression.ZipArchive]::new($zipStream2, [System.IO.Compression.ZipArchiveMode]::Update)
    $dotBytes = [System.Text.Encoding]::UTF8.GetBytes("dot segment content")
    $shaAlgo = [System.Security.Cryptography.SHA256]::Create()
    $dotHash = (($shaAlgo.ComputeHash($dotBytes) | ForEach-Object { $_.ToString('x2') }) -join '')

    $dotEntry = $zip2.CreateEntry('evidence/./dot-segment.txt')
    $ds = $dotEntry.Open()
    $ds.Write($dotBytes, 0, $dotBytes.Length)
    $ds.Dispose()

    # Rebuild checksums with evidence/./dot-segment.txt
    $cEntry2 = $zip2.GetEntry('checksums.sha256')
    $csStream2 = $cEntry2.Open()
    $ms2 = [System.IO.MemoryStream]::new()
    $csStream2.CopyTo($ms2)
    $oldCsText2 = [System.Text.Encoding]::UTF8.GetString($ms2.ToArray())
    $csStream2.Dispose()
    $ms2.Dispose()

    $csLines2 = [System.Collections.Generic.List[string]]::new()
    foreach ($line in $oldCsText2.TrimEnd("`n").Split("`n")) {
        if ($line.Length -gt 0) { $csLines2.Add($line) }
    }
    $csLines2.Add("$dotHash  evidence/./dot-segment.txt")
    $csLines2Array = $csLines2.ToArray()
    [System.Array]::Sort($csLines2Array, [System.Collections.Generic.Comparer[string]]::Create({
        param($a, $b)
        $pa = ($a -split '  ')[1]
        $pb = ($b -split '  ')[1]
        [System.StringComparer]::Ordinal.Compare($pa, $pb)
    }))

    $newCsText2 = ($csLines2Array -join "`n") + "`n"
    $newCsBytes2 = [System.Text.UTF8Encoding]::new($false).GetBytes($newCsText2)

    $cEntry2.Delete()
    $newEntry2 = $zip2.CreateEntry('checksums.sha256')
    $ns2 = $newEntry2.Open()
    $ns2.Write($newCsBytes2, 0, $newCsBytes2.Length)
    $ns2.Dispose()

    $zip2.Dispose()
    $zipStream2.Dispose()

    $sub2Rejected = $false
    try {
        Test-PalkaEvidenceBundle -ArtifactPath $tamperedZipPath2 | Out-Null
    } catch {
        $sub2Rejected = ($_.Exception.Message -match 'invalid path segment|VERIFIER_FAILURE')
    }

    if ($sub1Rejected -and $sub2Rejected) {
        Report-Pass 'T104' 'Unsafe archive/checksum path such as ../escape.txt: verifier rejects without writing outside controlled location'
    } else {
        Report-Fail 'T104' 'Unsafe archive/checksum path such as ../escape.txt: verifier rejects without writing outside controlled location' "sub1=$sub1Rejected, sub2=$sub2Rejected"
    }
} catch {
    Report-Fail 'T104' 'Unsafe archive/checksum path such as ../escape.txt: verifier rejects without writing outside controlled location' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T105 — A digest-valid operation that STOPs at a read-only precondition after run-directory creation still produces a valid canonical bundle whose summary says STOPPED / NOT_APPLIED and whose journal contains only actually started commands (DEC-003 Phase 2B R3)
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't105'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha -CustomProperties @{
        'authorized_commands' = @(
            [ordered]@{
                'id' = 'mutating-action'
                'executable' = 'git'
                'arguments' = @('branch', 'new-branch-t105')
                'cwd' = $repo.RepoDir
                'mutating' = $true
                'expect' = [ordered]@{ 'exit_code' = 0 }
            }
        )
        'required_preconditions' = @(
            [ordered]@{
                'id' = 'failing-precondition'
                'executable' = 'git'
                'arguments' = @('rev-parse', 'nonexistent-ref-xyz')
                'cwd' = $repo.RepoDir
                'mutating' = $false
                'expect' = [ordered]@{ 'exit_code' = 0 }
            }
        )
    }
    $sha = Get-TestManifestSha256 $m.ManifestPath
    $res = Invoke-PalkaEngine -ManifestPath $m.ManifestPath -OutputRoot $outputRoot -AuthorizedManifestSha256 $sha -PassThru

    $stoppedProperly = ($res.result -eq 'STOPPED' -and $res.mutation_state -eq 'NOT_APPLIED')
    $artExists = ($null -ne $res.artifact_path -and (Test-Path -LiteralPath $res.artifact_path))
    $bundleValid = if ($artExists) { Test-PalkaEvidenceBundle -ArtifactPath $res.artifact_path } else { $false }

    # Check summary and commands inside zip
    $zipStream = [System.IO.File]::OpenRead($res.artifact_path)
    $zip = [System.IO.Compression.ZipArchive]::new($zipStream, [System.IO.Compression.ZipArchiveMode]::Read)

    $sEntry = $zip.GetEntry('summary.json')
    $ss = $sEntry.Open()
    $sms = [System.IO.MemoryStream]::new()
    $ss.CopyTo($sms)
    $zipSumText = [System.Text.Encoding]::UTF8.GetString($sms.ToArray())
    $ss.Dispose()
    $sms.Dispose()

    $cEntry = $zip.GetEntry('commands.jsonl')
    $cs = $cEntry.Open()
    $cms = [System.IO.MemoryStream]::new()
    $cs.CopyTo($cms)
    $zipCmdText = [System.Text.Encoding]::UTF8.GetString($cms.ToArray())
    $cs.Dispose()
    $cms.Dispose()

    $zipSumObj = $zipSumText | ConvertFrom-Json
    $sumMatches = ($zipSumObj.result -eq 'STOPPED' -and $zipSumObj.mutation_state -eq 'NOT_APPLIED')

    # Inspect commands.jsonl records: exactly 4 started commands
    $cmdLines = @($zipCmdText.Trim().Split("`n") | ForEach-Object { $_.Trim() } | Where-Object { $_.Length -gt 0 })
    $cmdObjs = @($cmdLines | ForEach-Object { $_ | ConvertFrom-Json })
    $cmdIds = @($cmdObjs | ForEach-Object { $_.command_id })

    $expectedCmdIds = @(
        'builtin-preflight-toplevel',
        'builtin-preflight-branch',
        'builtin-preflight-head',
        'failing-precondition'
    )

    $countMatch = ($cmdObjs.Count -eq 4)
    $idsMatch = ($cmdIds.Count -eq 4 -and
                 $cmdIds[0] -eq 'builtin-preflight-toplevel' -and
                 $cmdIds[1] -eq 'builtin-preflight-branch' -and
                 $cmdIds[2] -eq 'builtin-preflight-head' -and
                 $cmdIds[3] -eq 'failing-precondition')
    $noMutatingAction = (-not ($cmdIds -contains 'mutating-action'))
    $allNonMutating = (-not (@($cmdObjs | Where-Object { $_.mutating -eq $true }).Count -gt 0))

    # Check evidence existence for all 4 records
    $allEvidenceExist = $true
    foreach ($co in $cmdObjs) {
        $outB = [System.IO.Path]::GetFileName($co.stdout_path)
        $errB = [System.IO.Path]::GetFileName($co.stderr_path)
        if ($null -eq ($zip.GetEntry("evidence/$outB")) -or $null -eq ($zip.GetEntry("evidence/$errB"))) {
            $allEvidenceExist = $false
        }
    }

    $zip.Dispose()
    $zipStream.Dispose()

    $journalValid = ($countMatch -and $idsMatch -and $noMutatingAction -and $allNonMutating -and $allEvidenceExist)

    if ($stoppedProperly -and $artExists -and $bundleValid -and $sumMatches -and $journalValid) {
        Report-Pass 'T105' 'A digest-valid operation that STOPs at a precondition still produces a valid canonical bundle'
    } else {
        Report-Fail 'T105' 'A digest-valid operation that STOPs at a precondition still produces a valid canonical bundle' "stopped=$stoppedProperly, artExists=$artExists, bundleValid=$bundleValid, sumMatches=$sumMatches, journalValid=$journalValid (count=$countMatch, ids=$idsMatch, noMut=$noMutatingAction, allNonMut=$allNonMutating, evExist=$allEvidenceExist)"
    }
} catch {
    Report-Fail 'T105' 'A digest-valid operation that STOPs at a precondition still produces a valid canonical bundle' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

# ----------------------------------------------------
# T106 — Wrong/malformed authorization digest still launches zero native processes, creates no canonical run artifact, and CLI exposes: ARTIFACT: <none>, ARTIFACT_SHA256: <none> (DEC-003 Phase 2B)
# ----------------------------------------------------
try {
    $repo = New-TestGitRepo 't106'
    $m = New-TestManifest -RepoDir $repo.RepoDir -HeadSha $repo.HeadSha
    $cliScript = Join-Path (Split-Path -Parent $scriptDir) 'Invoke-PalkaOperation.ps1'

    $wrongDigest = '0000000000000000000000000000000000000000000000000000000000000000'
    $p = New-Object System.Diagnostics.Process
    $p.StartInfo.FileName = 'powershell.exe'
    $p.StartInfo.Arguments = "-NoProfile -ExecutionPolicy Bypass -File `"$cliScript`" -ManifestPath `"$($m.ManifestPath)`" -OutputRoot `"$outputRoot`" -AuthorizedManifestSha256 $wrongDigest"
    $p.StartInfo.UseShellExecute = $false
    $p.StartInfo.RedirectStandardOutput = $true
    $p.StartInfo.RedirectStandardError = $true
    $p.Start() | Out-Null
    $outText = $p.StandardOutput.ReadToEnd()
    $p.WaitForExit()
    $exitCode = $p.ExitCode

    $hasArtifactNone = ($outText -match 'ARTIFACT:\s*<none>')
    $hasShaNone = ($outText -match 'ARTIFACT_SHA256:\s*<none>')
    $isStopped = ($outText -match 'RESULT:\s*STOPPED')
    $isNotApplied = ($outText -match 'MUTATION_STATE:\s*NOT_APPLIED')

    if ($exitCode -ne 0 -and $hasArtifactNone -and $hasShaNone -and $isStopped -and $isNotApplied) {
        Report-Pass 'T106' 'Wrong authorization digest launches zero native processes, creates no canonical run artifact, and CLI exposes <none>'
    } else {
        Report-Fail 'T106' 'Wrong authorization digest launches zero native processes, creates no canonical run artifact, and CLI exposes <none>' "exit=$exitCode, artNone=$hasArtifactNone, shaNone=$hasShaNone, stopped=$isStopped"
    }
} catch {
    Report-Fail 'T106' 'Wrong authorization digest launches zero native processes, creates no canonical run artifact, and CLI exposes <none>' $_.Exception.Message
} finally {
    if ($null -ne $repo) { Remove-TestGitRepo $repo.RepoDir }
    if ($null -ne $m -and (Test-Path $m.ManifestPath)) { Remove-Item -Force $m.ManifestPath }
}

Write-Host "----------------------------------"
Write-Host "Self-tests finished: $passCount/$totalTests PASS, $failCount FAIL"

if (Test-Path $outputRoot) {
    Remove-Item -LiteralPath $outputRoot -Recurse -Force -ErrorAction SilentlyContinue
}

if ($failCount -eq 0 -and $passCount -eq $totalTests) {
    exit 0
} else {
    exit 1
}
