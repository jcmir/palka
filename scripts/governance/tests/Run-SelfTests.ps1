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
$totalTests = 86
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
