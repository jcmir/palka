# PalkaGovernance.psm1
# Governance Execution Engine Core (DEC-003 Phase 2A R3)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# R2-02: Deterministic Failure Exception Class
class PalkaEngineException : System.Exception {
    [string]$FailureKind
    [string]$CommandId
    [string]$Phase

    PalkaEngineException([string]$failureKind, [string]$message) : base($message) {
        $this.FailureKind = $failureKind
        $this.CommandId = $null
        $this.Phase = $null
    }

    PalkaEngineException([string]$failureKind, [string]$commandId, [string]$phase, [string]$message) : base($message) {
        $this.FailureKind = $failureKind
        $this.CommandId = $commandId
        $this.Phase = $phase
    }
}

function Format-PalkaProcessArgument {
    [CmdletBinding()]
    param (
        [Parameter(Mandatory = $true)]
        [AllowEmptyString()]
        [string]$Argument
    )

    if ($Argument.Length -eq 0) {
        return '""'
    }

    # If argument contains no spaces, tabs, newlines, or quotes, return as-is
    if ($Argument -notmatch '[\s\t\n\r"]') {
        return $Argument
    }

    # MS CRT CommandLineToArgvW quoting algorithm
    $sb = New-Object System.Text.StringBuilder
    [void]$sb.Append('"')
    $backslashes = 0

    for ($i = 0; $i -lt $Argument.Length; $i++) {
        $c = $Argument[$i]
        if ($c -eq '\') {
            $backslashes++
        }
        elseif ($c -eq '"') {
            # 2n + 1 backslashes followed by quote
            [void]$sb.Append('\' * (2 * $backslashes + 1))
            [void]$sb.Append('"')
            $backslashes = 0
        }
        else {
            if ($backslashes -gt 0) {
                [void]$sb.Append('\' * $backslashes)
                $backslashes = 0
            }
            [void]$sb.Append($c)
        }
    }

    # Escape trailing backslashes before the closing quote: 2n backslashes
    if ($backslashes -gt 0) {
        [void]$sb.Append('\' * (2 * $backslashes))
    }
    [void]$sb.Append('"')

    return $sb.ToString()
}

function Test-PalkaSha40 {
    param ([string]$Sha)
    if ($null -eq $Sha) { return $false }
    return ($Sha -match '^[0-9a-f]{40}$')
}

function Test-PalkaSafeIdentifier {
    param ([string]$Id)
    if ($null -eq $Id -or $Id.Length -eq 0 -or $Id.Length -gt 128) { return $false }
    if ($Id -eq '.' -or $Id -eq '..') { return $false }
    return ($Id -match '^[A-Za-z0-9][A-Za-z0-9._-]*$')
}

function Test-PalkaSafeCommandId {
    param ([string]$Id)
    if ($null -eq $Id -or $Id.Length -eq 0 -or $Id.Length -gt 128) { return $false }
    if ($Id -eq '.' -or $Id -eq '..') { return $false }
    return ($Id -match '^[A-Za-z0-9_-]+$')
}

function Test-PalkaGlobMatch {
    param (
        [string]$Path,
        [string]$Pattern
    )
    # Normalize slashes
    $normPath = $Path.Replace('\', '/')
    $normPattern = $Pattern.Replace('\', '/')

    # Convert glob pattern to regex
    $regexPattern = '^' + [regex]::Escape($normPattern).Replace('\*\*', '.*').Replace('\*', '[^/]*').Replace('\?', '.') + '$'
    return ($normPath -match $regexPattern)
}

function Test-PalkaRefreshPolicy {
    [CmdletBinding()]
    param (
        [Parameter(Mandatory = $false)]
        $Arguments
    )

    $argsList = [System.Collections.Generic.List[string]]::new()
    if ($null -ne $Arguments) {
        if ($Arguments -is [string]) {
            $argsList.Add($Arguments)
        }
        else {
            foreach ($a in $Arguments) {
                if ($null -ne $a) {
                    $argsList.Add([string]$a)
                }
            }
        }
    }

    $fetchArgs = if ($argsList.Count -gt 0 -and $argsList[0] -eq 'fetch') {
        $argsList.GetRange(1, $argsList.Count - 1)
    }
    else {
        $argsList
    }

    foreach ($arg in $fetchArgs) {
        $norm = $arg.ToLowerInvariant().Trim()
        if ($norm -in @('--force', '-f', '--prune', '-p', '--prune-tags') -or
            $norm.StartsWith('--force=') -or
            $norm.StartsWith('-f=') -or
            $norm.StartsWith('--prune=') -or
            $norm.StartsWith('-p=') -or
            $norm.StartsWith('--prune-tags=')) {
            return "Forbidden force/prune flag in refresh_commands: '$arg'"
        }
        if ($arg.StartsWith('+') -or $arg.StartsWith('*')) {
            return "Forbidden force refspec in refresh_commands: '$arg'"
        }
        # R3-06: Reject force refmaps in options
        if ($norm.StartsWith('--refmap=+') -or ($norm.StartsWith('--refmap=') -and $norm.Contains('+'))) {
            return "Forbidden force refmap in refresh_commands: '$arg'"
        }
    }

    return $null
}

function Test-PalkaDangerousPolicy {
    [CmdletBinding()]
    param (
        [Parameter(Mandatory = $true)]
        [string]$Executable,

        [Parameter(Mandatory = $false)]
        $Arguments
    )

    if ($null -eq $Executable) {
        return "Executable cannot be null"
    }

    $exeName = [System.IO.Path]::GetFileNameWithoutExtension($Executable).ToLowerInvariant()
    $argsList = [System.Collections.Generic.List[string]]::new()
    if ($null -ne $Arguments) {
        if ($Arguments -is [string]) {
            $argsList.Add($Arguments)
        }
        else {
            foreach ($a in $Arguments) {
                if ($null -ne $a) {
                    $argsList.Add([string]$a)
                }
            }
        }
    }

    # R1-07: Reject opaque shell wrappers
    if ($exeName -in @('cmd', 'cmd.exe')) {
        foreach ($arg in $argsList) {
            $norm = $arg.ToLowerInvariant().Trim()
            if ($norm -in @('/c', '-c', '/k', '-k') -or $norm.StartsWith('/c:') -or $norm.StartsWith('/k:') -or $norm.StartsWith('-c:') -or $norm.StartsWith('-k:')) {
                return "Opaque shell wrapper rejected: cmd with /c or /k flag"
            }
        }
    }

    # R2-12, R3-04: PowerShell abbreviation hardening (-Command, -CommandWithArgs, -EncodedCommand)
    if ($exeName -in @('powershell', 'powershell.exe', 'pwsh', 'pwsh.exe')) {
        $i = 0
        while ($i -lt $argsList.Count) {
            $arg = $argsList[$i]
            $norm = $arg.ToLowerInvariant().Trim()

            # Structured file invocation: parameters after script path are script arguments
            if ($norm -in @('-file', '/file', '-f', '/f', '-fi', '/fi', '-fil', '/fil')) {
                break
            }
            if ($norm.StartsWith('-file:') -or $norm.StartsWith('/file:') -or
                $norm.StartsWith('-f:') -or $norm.StartsWith('/f:') -or
                $norm.StartsWith('-fi:') -or $norm.StartsWith('/fi:') -or
                $norm.StartsWith('-fil:') -or $norm.StartsWith('/fil:')) {
                break
            }

            if ($norm.StartsWith('-') -or $norm.StartsWith('/')) {
                $switchBody = $norm.Substring(1)
                if ($switchBody.Contains(':')) {
                    $switchBody = $switchBody.Substring(0, $switchBody.IndexOf(':'))
                }

                # -Command and -CommandWithArgs abbreviations
                if ($switchBody -in @('c', 'co', 'com', 'comm', 'comma', 'comman', 'command') -or
                    $switchBody -in @('commandw', 'commandwi', 'commandwit', 'commandwith', 'commandwitha', 'commandwithar', 'commandwitharg', 'commandwithargs') -or
                    $switchBody.StartsWith('commandw')) {
                    return "Opaque shell wrapper rejected: PowerShell command string execution ($arg)"
                }

                # -EncodedCommand variants
                if ($switchBody -in @('e', 'en', 'enc', 'enco', 'encod', 'encode', 'encoded', 'encodedc', 'encodedco', 'encodedcom', 'encodedcomm', 'encodedcomma', 'encodedcomman', 'encodedcommand', 'ec') -or
                    $switchBody.StartsWith('encodedc')) {
                    return "Opaque shell wrapper rejected: PowerShell encoded command execution ($arg)"
                }
            }
            $i++
        }
    }

    if ($exeName -eq 'git') {
        # Parse git arguments skipping global options to find subcommand
        $globalOptsWithArg = @('-c', '-C', '--git-dir', '--work-tree', '--namespace', '--config-env', '--exec-path')
        $subCommand = $null
        $subCommandIndex = -1

        $i = 0
        while ($i -lt $argsList.Count) {
            $arg = $argsList[$i]

            # R1-06: Reject git alias configuration bypass
            if ($arg -match '^-c\s*alias\.' -or $arg -match '^--config-env\s*=\s*alias\.') {
                return "Forbidden git alias configuration: '$arg'"
            }
            if ($arg -eq '-c' -and ($i + 1) -lt $argsList.Count) {
                if ($argsList[$i + 1] -match '^alias\.') {
                    return "Forbidden git alias configuration: -c $($argsList[$i + 1])"
                }
            }
            if ($arg -eq '--config-env' -and ($i + 1) -lt $argsList.Count) {
                if ($argsList[$i + 1] -match '^alias\.') {
                    return "Forbidden git alias configuration: --config-env $($argsList[$i + 1])"
                }
            }

            # Check if global option with separate arg
            if ($arg -in $globalOptsWithArg) {
                $i += 2
                continue
            }

            # Check if global option with = (e.g. --git-dir=foo, -Cpath)
            if ($arg.StartsWith('--git-dir=') -or $arg.StartsWith('--work-tree=') -or
                $arg.StartsWith('--namespace=') -or $arg.StartsWith('--config-env=') -or
                $arg.StartsWith('-C') -or $arg.StartsWith('-c') -or $arg.StartsWith('--exec-path=')) {
                $i++
                continue
            }

            # Global flags without args
            if ($arg.StartsWith('--') -or $arg.StartsWith('-')) {
                $i++
                continue
            }

            # First non-option token is the subcommand
            $subCommand = $arg.ToLowerInvariant()
            $subCommandIndex = $i
            break
        }

        if ($null -ne $subCommand) {
            if ($subCommand -in @('reset', 'clean', 'rebase', 'merge')) {
                return "Globally forbidden git subcommand: git $subCommand"
            }

            # R3-05: Safely extract subcommand arguments without out-of-range index
            $subArgs = if ($subCommandIndex -lt ($argsList.Count - 1)) {
                $argsList.GetRange($subCommandIndex + 1, $argsList.Count - ($subCommandIndex + 1))
            }
            else {
                [System.Collections.Generic.List[string]]::new()
            }

            if ($subCommand -eq 'commit') {
                foreach ($sarg in $subArgs) {
                    if ($sarg -eq '--amend') {
                        return 'Globally forbidden git commit --amend'
                    }
                }
            }

            if ($subCommand -eq 'push') {
                foreach ($sarg in $subArgs) {
                    $sargLow = $sarg.ToLowerInvariant()
                    if ($sargLow -in @('--force', '-f', '--force-with-lease', '--delete', '--mirror', '--prune') -or
                        $sargLow.StartsWith('--force=') -or
                        $sargLow.StartsWith('--force-with-lease=') -or
                        $sargLow.StartsWith('--delete=') -or
                        $sargLow.StartsWith('--mirror=') -or
                        $sargLow.StartsWith('--prune=')) {
                        return "Globally forbidden git push flag: $sarg"
                    }
                    if ($sarg.StartsWith('+')) {
                        return "Globally forbidden git push force refspec: $sarg"
                    }
                }
            }

            if ($subCommand -eq 'branch') {
                foreach ($sarg in $subArgs) {
                    if ($sarg -in @('-D', '-d', '--delete')) {
                        return "Globally forbidden git branch deletion flag: $sarg"
                    }
                }
            }

            if ($subCommand -eq 'checkout') {
                foreach ($sarg in $subArgs) {
                    if ($sarg -in @('-f', '--force')) {
                        return "Globally forbidden git checkout force flag: $sarg"
                    }
                }
            }

            if ($subCommand -eq 'switch') {
                foreach ($sarg in $subArgs) {
                    if ($sarg -eq '--discard-changes') {
                        return 'Globally forbidden git switch flag: --discard-changes'
                    }
                }
            }
        }
    }

    return $null
}

function Get-PalkaPorcelainZPaths {
    param ([string]$RawStatusText)

    $paths = [System.Collections.Generic.List[string]]::new()
    if ([string]::IsNullOrEmpty($RawStatusText)) {
        return $paths
    }

    $tokens = $RawStatusText.Split([char]0)
    $i = 0
    while ($i -lt $tokens.Length) {
        $token = $tokens[$i]
        if ($token.Length -eq 0) {
            $i++
            continue
        }

        if ($token.Length -lt 3) {
            $i++
            continue
        }

        $x = $token[0]
        $y = $token[1]
        # R2-09: Do NOT trim path!
        $path1 = $token.Substring(3)
        $paths.Add($path1)

        # For rename/copy, the subsequent token is the source/original path
        if ($x -in @([char]'R', [char]'C') -or $y -in @([char]'R', [char]'C')) {
            $i++
            if ($i -lt $tokens.Length -and $tokens[$i].Length -gt 0) {
                $path2 = $tokens[$i]
                $paths.Add($path2)
            }
        }
        $i++
    }

    return $paths
}

function Test-PalkaManifestStructure {
    [CmdletBinding()]
    param (
        [Parameter(Mandatory = $true)]
        [psobject]$ManifestObject
    )

    $requiredTopLevel = @(
        'schema',
        'operation_id',
        'repository',
        'working_directory',
        'stage',
        'branch',
        'expected_start_branch',
        'target_branch',
        'expected_head',
        'expected_base',
        'expected_remote_refs',
        'authorized_paths',
        'forbidden_paths',
        'branch_transition',
        'refresh_commands',
        'required_preconditions',
        'already_satisfied_checks',
        'authorized_commands',
        'required_postconditions',
        'artifact_profile',
        'stop_conditions'
    )

    $actualProperties = @($ManifestObject.PSObject.Properties | ForEach-Object { $_.Name })
    foreach ($prop in $actualProperties) {
        if ($prop -notin $requiredTopLevel) {
            throw [PalkaEngineException]::new('ENGINE_FAILURE', "Unknown top-level field in manifest: '$prop'")
        }
    }
    foreach ($req in $requiredTopLevel) {
        if ($req -notin $actualProperties) {
            throw [PalkaEngineException]::new('ENGINE_FAILURE', "Missing required top-level field in manifest: '$req'")
        }
    }

    # R2-04: Strict JSON string types for top-level fields
    $stringTopLevelFields = @(
        'schema',
        'operation_id',
        'repository',
        'working_directory',
        'stage',
        'branch',
        'expected_start_branch',
        'target_branch',
        'expected_head',
        'expected_base',
        'artifact_profile'
    )
    foreach ($stf in $stringTopLevelFields) {
        $val = $ManifestObject.$stf
        if ($null -eq $val -or $val -isnot [string]) {
            throw [PalkaEngineException]::new('ENGINE_FAILURE', "Top-level field '$stf' must be a JSON string")
        }
    }

    if ($ManifestObject.schema -ne 'palka.operation-manifest/v1') {
        throw [PalkaEngineException]::new('ENGINE_FAILURE', "Unsupported manifest schema: '$($ManifestObject.schema)'")
    }

    # Validate operation_id
    if (-not (Test-PalkaSafeIdentifier $ManifestObject.operation_id)) {
        throw [PalkaEngineException]::new('ENGINE_FAILURE', "Invalid operation_id: '$($ManifestObject.operation_id)' (must match ^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$ and not be relative path navigation)")
    }

    $validStages = @(
        'IMPLEMENTATION',
        'DOCUMENTATION_IMPLEMENTATION',
        'COMMIT',
        'PUSH',
        'PR_CREATE',
        'CI_INSPECTION',
        'RERUN',
        'MERGE',
        'GOVERNANCE_SYNC',
        'REPOSITORY_POLICY_CHANGE',
        'RELEASE'
    )
    if ($ManifestObject.stage -notin $validStages) {
        throw [PalkaEngineException]::new('ENGINE_FAILURE', "Invalid stage: '$($ManifestObject.stage)'")
    }

    # Branch validation
    $bt = $ManifestObject.branch_transition
    if ($null -eq $bt -or $bt -isnot [psobject]) {
        throw [PalkaEngineException]::new('ENGINE_FAILURE', 'branch_transition must be a JSON object')
    }
    $btProps = @($bt.PSObject.Properties | ForEach-Object { $_.Name })
    if ('allowed' -notin $btProps) {
        throw [PalkaEngineException]::new('ENGINE_FAILURE', 'branch_transition.allowed must be defined')
    }
    if ($bt.allowed -isnot [bool] -and $bt.allowed -isnot [System.Boolean]) {
        throw [PalkaEngineException]::new('ENGINE_FAILURE', 'branch_transition.allowed must be a strict JSON boolean')
    }

    if ($bt.allowed -eq $false) {
        $allowedBtProps = @('allowed')
        foreach ($p in $btProps) {
            if ($p -notin $allowedBtProps) {
                throw [PalkaEngineException]::new('ENGINE_FAILURE', "Unknown property '$p' in branch_transition when allowed is false")
            }
        }
        if ($ManifestObject.expected_start_branch -ne $ManifestObject.target_branch -or
            $ManifestObject.target_branch -ne $ManifestObject.branch) {
            throw [PalkaEngineException]::new('ENGINE_FAILURE', 'When branch_transition.allowed is false, expected_start_branch, target_branch and branch must be identical')
        }
    }
    elseif ($bt.allowed -eq $true) {
        $allowedBtProps = @('allowed', 'mode', 'from', 'to')
        foreach ($p in $btProps) {
            if ($p -notin $allowedBtProps) {
                throw [PalkaEngineException]::new('ENGINE_FAILURE', "Unknown property '$p' in branch_transition")
            }
        }
        foreach ($reqP in @('mode', 'from', 'to')) {
            if ($reqP -notin $btProps -or $bt.$reqP -isnot [string]) {
                throw [PalkaEngineException]::new('ENGINE_FAILURE', "branch_transition.$reqP must be a JSON string when allowed is true")
            }
        }
        if ($bt.mode -notin @('create', 'switch')) {
            throw [PalkaEngineException]::new('ENGINE_FAILURE', "branch_transition.mode must be 'create' or 'switch'")
        }
        if ($bt.from -ne $ManifestObject.expected_start_branch) {
            throw [PalkaEngineException]::new('ENGINE_FAILURE', "branch_transition.from ('$($bt.from)') must match expected_start_branch ('$($ManifestObject.expected_start_branch)')")
        }
        if ($bt.to -ne $ManifestObject.target_branch) {
            throw [PalkaEngineException]::new('ENGINE_FAILURE', "branch_transition.to ('$($bt.to)') must match target_branch ('$($ManifestObject.target_branch)')")
        }
        if ($ManifestObject.branch -ne $ManifestObject.target_branch) {
            throw [PalkaEngineException]::new('ENGINE_FAILURE', "branch ('$($ManifestObject.branch)') must match target_branch ('$($ManifestObject.target_branch)')")
        }
    }

    # SHA validation
    if (-not (Test-PalkaSha40 $ManifestObject.expected_head)) {
        throw [PalkaEngineException]::new('ENGINE_FAILURE', "expected_head must be exactly 40 lowercase hex characters: '$($ManifestObject.expected_head)'")
    }
    if (-not (Test-PalkaSha40 $ManifestObject.expected_base)) {
        throw [PalkaEngineException]::new('ENGINE_FAILURE', "expected_base must be exactly 40 lowercase hex characters: '$($ManifestObject.expected_base)'")
    }

    # Remote refs validation
    if ($null -ne $ManifestObject.expected_remote_refs) {
        if ($ManifestObject.expected_remote_refs -isnot [psobject]) {
            throw [PalkaEngineException]::new('ENGINE_FAILURE', 'expected_remote_refs must be a JSON object')
        }
        foreach ($prop in $ManifestObject.expected_remote_refs.PSObject.Properties) {
            $refName = $prop.Name
            if (-not ($refName.StartsWith('origin/'))) {
                throw [PalkaEngineException]::new('ENGINE_FAILURE', "expected_remote_refs keys must be in 'origin/<branch-name>' format: '$refName'")
            }
            $val = $prop.Value
            if ($val -isnot [string] -or ($val -ne 'ABSENT' -and -not (Test-PalkaSha40 $val))) {
                throw [PalkaEngineException]::new('ENGINE_FAILURE', "expected_remote_refs['$refName'] must be 40 lowercase hex characters or ABSENT: '$val'")
            }
        }
    }

    # R2-04, R2-62: authorized_paths and forbidden_paths validation
    foreach ($pathField in @('authorized_paths', 'forbidden_paths')) {
        $arr = $ManifestObject.$pathField
        if ($null -eq $arr -or ($arr -isnot [System.Array] -and $arr -isnot [System.Collections.IList])) {
            throw [PalkaEngineException]::new('ENGINE_FAILURE', "Field '$pathField' must be a JSON array")
        }
        foreach ($item in $arr) {
            if ($null -eq $item -or $item -isnot [string]) {
                throw [PalkaEngineException]::new('ENGINE_FAILURE', "Field '$pathField' must contain only JSON strings")
            }
        }
    }

    # Validate command arrays
    $commandSections = @('refresh_commands', 'required_preconditions', 'already_satisfied_checks', 'authorized_commands', 'required_postconditions')
    $seenCommandIds = @{}

    foreach ($sec in $commandSections) {
        $cmds = $ManifestObject.$sec
        if ($null -eq $cmds -or ($cmds -isnot [System.Array] -and $cmds -isnot [System.Collections.IList])) {
            throw [PalkaEngineException]::new('ENGINE_FAILURE', "Command section '$sec' must be a JSON array")
        }

        foreach ($cmd in $cmds) {
            if ($cmd -isnot [psobject]) {
                throw [PalkaEngineException]::new('ENGINE_FAILURE', "Command entry in section '$sec' must be a JSON object")
            }

            $cmdProps = @($cmd.PSObject.Properties | ForEach-Object { $_.Name })
            $requiredCmdProps = @('id', 'executable', 'arguments', 'cwd', 'mutating', 'expect')
            foreach ($p in $cmdProps) {
                if ($p -notin $requiredCmdProps) {
                    throw [PalkaEngineException]::new('ENGINE_FAILURE', "Unknown command property '$p' in section '$sec', command '$($cmd.id)'")
                }
            }
            foreach ($reqP in $requiredCmdProps) {
                if ($reqP -notin $cmdProps) {
                    throw [PalkaEngineException]::new('ENGINE_FAILURE', "Missing required command property '$reqP' in section '$sec'")
                }
            }

            # String property checks
            if ($cmd.id -isnot [string]) {
                throw [PalkaEngineException]::new('ENGINE_FAILURE', "Command id must be a JSON string in section '$sec'")
            }
            if ($cmd.executable -isnot [string]) {
                throw [PalkaEngineException]::new('ENGINE_FAILURE', "Command executable must be a JSON string in command '$($cmd.id)'")
            }
            if ($cmd.cwd -isnot [string]) {
                throw [PalkaEngineException]::new('ENGINE_FAILURE', "Command cwd must be a JSON string in command '$($cmd.id)'")
            }

            # R2-06: Reserve builtin- namespace
            if ($cmd.id.StartsWith('builtin-', [System.StringComparison]::OrdinalIgnoreCase)) {
                throw [PalkaEngineException]::new('ENGINE_FAILURE', "Manifest command id cannot start with 'builtin-': '$($cmd.id)'")
            }

            if (-not (Test-PalkaSafeCommandId $cmd.id)) {
                throw [PalkaEngineException]::new('ENGINE_FAILURE', "Invalid command id: '$($cmd.id)' in section '$sec' (must match ^[A-Za-z0-9_-]+$)")
            }

            if ($seenCommandIds.ContainsKey($cmd.id)) {
                throw [PalkaEngineException]::new('ENGINE_FAILURE', "Duplicate command id: '$($cmd.id)'")
            }
            $seenCommandIds[$cmd.id] = $true

            if ($cmd.mutating -isnot [bool] -and $cmd.mutating -isnot [System.Boolean]) {
                throw [PalkaEngineException]::new('ENGINE_FAILURE', "Property 'mutating' must be a strict JSON boolean in section '$sec', command '$($cmd.id)'")
            }

            if ($sec -ne 'authorized_commands' -and $cmd.mutating -ne $false) {
                throw [PalkaEngineException]::new('ENGINE_FAILURE', "Commands in section '$sec' must have mutating: false (command '$($cmd.id)')")
            }

            if ($cmd.arguments -isnot [System.Array] -and $cmd.arguments -isnot [System.Collections.IList]) {
                throw [PalkaEngineException]::new('ENGINE_FAILURE', "arguments must be a JSON array in command '$($cmd.id)'")
            }
            foreach ($arg in $cmd.arguments) {
                if ($null -eq $arg -or $arg -isnot [string]) {
                    throw [PalkaEngineException]::new('ENGINE_FAILURE', "arguments in command '$($cmd.id)' must contain only JSON strings")
                }
            }

            # R2-05: Expect validation
            $exp = $cmd.expect
            if ($null -eq $exp -or $exp -isnot [psobject]) {
                throw [PalkaEngineException]::new('ENGINE_FAILURE', "expect must be a JSON object in command '$($cmd.id)'")
            }
            $allowedExpProps = @('exit_code', 'stdout_equals', 'stdout_empty', 'stderr_equals', 'stderr_empty')
            $expProps = @($exp.PSObject.Properties | ForEach-Object { $_.Name })
            foreach ($ep in $expProps) {
                if ($ep -notin $allowedExpProps) {
                    throw [PalkaEngineException]::new('ENGINE_FAILURE', "Unknown property in expect object: '$ep' in command '$($cmd.id)'")
                }
            }
            if ('exit_code' -notin $expProps) {
                throw [PalkaEngineException]::new('ENGINE_FAILURE', "expect.exit_code must be specified in command '$($cmd.id)'")
            }
            if ($exp.exit_code -isnot [int] -and $exp.exit_code -isnot [long] -and $exp.exit_code -isnot [System.Int32]) {
                throw [PalkaEngineException]::new('ENGINE_FAILURE', "expect.exit_code must be an integer in command '$($cmd.id)'")
            }
            if ('stdout_equals' -in $expProps) {
                if ($exp.stdout_equals -isnot [string]) {
                    throw [PalkaEngineException]::new('ENGINE_FAILURE', "expect.stdout_equals must be a JSON string in command '$($cmd.id)'")
                }
            }
            if ('stdout_empty' -in $expProps) {
                if ($exp.stdout_empty -isnot [bool] -and $exp.stdout_empty -isnot [System.Boolean]) {
                    throw [PalkaEngineException]::new('ENGINE_FAILURE', "expect.stdout_empty must be a strict JSON boolean in command '$($cmd.id)'")
                }
            }
            if ('stderr_equals' -in $expProps) {
                if ($exp.stderr_equals -isnot [string]) {
                    throw [PalkaEngineException]::new('ENGINE_FAILURE', "expect.stderr_equals must be a JSON string in command '$($cmd.id)'")
                }
            }
            if ('stderr_empty' -in $expProps) {
                if ($exp.stderr_empty -isnot [bool] -and $exp.stderr_empty -isnot [System.Boolean]) {
                    throw [PalkaEngineException]::new('ENGINE_FAILURE', "expect.stderr_empty must be a strict JSON boolean in command '$($cmd.id)'")
                }
            }
            if ('stdout_equals' -in $expProps -and 'stdout_empty' -in $expProps) {
                throw [PalkaEngineException]::new('ENGINE_FAILURE', "stdout_equals and stdout_empty are mutually exclusive in command '$($cmd.id)'")
            }
            if ('stderr_equals' -in $expProps -and 'stderr_empty' -in $expProps) {
                throw [PalkaEngineException]::new('ENGINE_FAILURE', "stderr_equals and stderr_empty are mutually exclusive in command '$($cmd.id)'")
            }

            if ($sec -eq 'refresh_commands') {
                $exeName = [System.IO.Path]::GetFileNameWithoutExtension($cmd.executable).ToLowerInvariant()
                if ($exeName -ne 'git') {
                    throw [PalkaEngineException]::new('ENGINE_FAILURE', "refresh_commands executable must be git (command '$($cmd.id)')")
                }
                if ($null -eq $cmd.arguments -or $cmd.arguments.Count -eq 0 -or $cmd.arguments[0] -ne 'fetch') {
                    throw [PalkaEngineException]::new('ENGINE_FAILURE', "refresh_commands must be git fetch (command '$($cmd.id)')")
                }
                $rfErr = Test-PalkaRefreshPolicy -Arguments $cmd.arguments
                if ($null -ne $rfErr) {
                    throw [PalkaEngineException]::new('POLICY_FAILURE', "$rfErr (command '$($cmd.id)')")
                }
            }
        }
    }
}

function Invoke-PalkaNativeProcess {
    param (
        [string]$Executable,
        [string[]]$Arguments,
        [string]$Cwd,
        [string]$StdoutPath,
        [string]$StderrPath,
        [string]$CommandId = $null,
        [string]$Phase = $null,
        [scriptblock]$OnStarted = $null,
        [scriptblock]$TestPostStartHook = $null
    )

    # R2-07: Ensure evidence files exist as zero bytes beforehand
    $zeroBytes = [byte[]]@()
    [System.IO.File]::WriteAllBytes($StdoutPath, $zeroBytes)
    [System.IO.File]::WriteAllBytes($StderrPath, $zeroBytes)

    $proc = New-Object System.Diagnostics.Process
    $proc.StartInfo.FileName = $Executable
    $proc.StartInfo.WorkingDirectory = $Cwd
    $proc.StartInfo.UseShellExecute = $false
    $proc.StartInfo.RedirectStandardOutput = $true
    $proc.StartInfo.RedirectStandardError = $true
    $proc.StartInfo.CreateNoWindow = $true

    $argsList = if ($null -ne $Arguments) { @($Arguments) } else { @() }

    # R1-16: Check for ProcessStartInfo.ArgumentList availability (.NET Core / PS Core)
    $hasArgList = $proc.StartInfo.PSObject.Properties['ArgumentList'] -ne $null
    if ($hasArgList) {
        $proc.StartInfo.ArgumentList.Clear()
        foreach ($arg in $argsList) {
            $proc.StartInfo.ArgumentList.Add($arg)
        }
    }
    else {
        # Fallback to MS CRT CommandLineToArgvW quoting
        $formattedArgs = @($argsList | ForEach-Object { Format-PalkaProcessArgument $_ }) -join ' '
        $proc.StartInfo.Arguments = $formattedArgs
    }

    $startTime = [DateTime]::UtcNow

    try {
        $started = $proc.Start()
        if (-not $started) {
            throw [PalkaEngineException]::new('LAUNCH_FAILURE', "Process failed to start: $Executable")
        }
    }
    catch {
        $endTime = [DateTime]::UtcNow
        return [PSCustomObject]@{
            Launched = $false
            LaunchError = $_.Exception.Message
            EngineError = $null
            ExitCode = $null
            StartTimeUtc = $startTime.ToString('o')
            EndTimeUtc = $endTime.ToString('o')
            StdoutText = ''
            StderrText = ''
        }
    }

    # R2-08: Trigger onStarted immediately upon successful process launch
    if ($null -ne $OnStarted) {
        try {
            & $OnStarted
        }
        catch {}
    }

    # R3-02, R3-03: Process stream reading and evidence capture with post-start failure resilience
    try {
        if ($null -ne $TestPostStartHook) {
            & $TestPostStartHook $CommandId $Phase $proc
        }

        # R1-15: Process stream deadlock hardening (asynchronous ReadToEndAsync)
        $stdoutTask = $proc.StandardOutput.ReadToEndAsync()
        $stderrTask = $proc.StandardError.ReadToEndAsync()

        $proc.WaitForExit()
        $stdoutTask.Wait()
        $stderrTask.Wait()

        $endTime = [DateTime]::UtcNow
        $exitCode = $proc.ExitCode
        $stdoutText = $stdoutTask.Result
        $stderrText = $stderrTask.Result

        # Save to disk as exact captured streams (UTF-8 no BOM)
        $utf8NoBom = [System.Text.UTF8Encoding]::new($false)
        [System.IO.File]::WriteAllBytes($StdoutPath, $utf8NoBom.GetBytes($stdoutText))
        [System.IO.File]::WriteAllBytes($StderrPath, $utf8NoBom.GetBytes($stderrText))

        return [PSCustomObject]@{
            Launched = $true
            LaunchError = $null
            EngineError = $null
            ExitCode = $exitCode
            StartTimeUtc = $startTime.ToString('o')
            EndTimeUtc = $endTime.ToString('o')
            StdoutText = $stdoutText
            StderrText = $stderrText
        }
    }
    catch {
        $endTime = [DateTime]::UtcNow
        $exitCode = try { if ($proc.HasExited) { $proc.ExitCode } else { $null } } catch { $null }
        return [PSCustomObject]@{
            Launched = $true
            LaunchError = $null
            EngineError = $_.Exception.Message
            ExitCode = $exitCode
            StartTimeUtc = $startTime.ToString('o')
            EndTimeUtc = $endTime.ToString('o')
            StdoutText = ''
            StderrText = ''
        }
    }
}

function Normalize-PalkaStreamText {
    param ([string]$Text)
    if ($null -eq $Text) { return '' }
    if ($Text.EndsWith("`r`n")) {
        return $Text.Substring(0, $Text.Length - 2)
    }
    elseif ($Text.EndsWith("`n")) {
        return $Text.Substring(0, $Text.Length - 1)
    }
    return $Text
}

function Invoke-PalkaEngine {
    [CmdletBinding()]
    param (
        [Parameter(Mandatory = $true)]
        [string]$ManifestPath,

        [Parameter(Mandatory = $true)]
        [string]$OutputRoot,

        [switch]$PassThru,

        # Internal test-only hook (R3-03); disabled in production defaults
        [scriptblock]$TestPostStartHook = $null
    )

    $startedAt = [DateTime]::UtcNow
    $runId = [Guid]::NewGuid().ToString('N')

    $engineState = [PSCustomObject]@{
        Sequence = 0
        MutationState = 'NONE'
        ProvenBranch = $null
        ProvenHead = $null
    }

    # Defaults
    $operationId = 'INVALID-MANIFEST'
    $result = 'STOPPED'
    $failedPhase = 'MANIFEST_READ'
    $failedCommandId = $null
    $failureReason = $null
    $manifest = $null
    $rawManifestBytes = $null
    $runDir = $null
    $commandsJournalPath = $null
    $summaryPath = $null
    $evidenceDir = $null

    $executedRecords = [System.Collections.Generic.List[psobject]]::new()
    $runSeenCommandIds = [System.Collections.Generic.HashSet[string]]::new([System.StringComparer]::OrdinalIgnoreCase)

    $writeCommandRecord = {
        param ($rec)
        $executedRecords.Add($rec)
        if ($null -ne $commandsJournalPath) {
            $json = $rec | ConvertTo-Json -Compress
            [System.IO.File]::AppendAllText($commandsJournalPath, $json + "`n", [System.Text.UTF8Encoding]::new($false))
        }
    }

    # R2-01: Unified journaled execution function (ABSOLUTE SINGLE NATIVE EXECUTION PATH)
    $executeJournaledProcess = {
        param (
            [string]$CommandId,
            [string]$Phase,
            [string]$Executable,
            [string[]]$Arguments,
            [string]$Cwd,
            [bool]$Mutating,
            $Expect
        )

        if (-not (Test-PalkaSafeCommandId $CommandId)) {
            throw [PalkaEngineException]::new('ENGINE_FAILURE', $CommandId, $Phase, "Invalid command_id '$CommandId' (must match ^[A-Za-z0-9_-]+$)")
        }

        # R2-06: Runtime check for duplicate command ID in run
        if ($runSeenCommandIds.Contains($CommandId)) {
            throw [PalkaEngineException]::new('ENGINE_FAILURE', $CommandId, $Phase, "Duplicate command_id in execution run: '$CommandId'")
        }
        $runSeenCommandIds.Add($CommandId) | Out-Null

        # Policy checks before launch
        $dang = Test-PalkaDangerousPolicy -Executable $Executable -Arguments $Arguments
        if ($null -ne $dang) {
            throw [PalkaEngineException]::new('POLICY_FAILURE', $CommandId, $Phase, "Policy violation: $dang")
        }

        $engineState.Sequence++
        $seqStr = "{0:D3}" -f $engineState.Sequence
        $stdoutFile = Join-Path $evidenceDir "$seqStr-$CommandId-stdout.txt"
        $stderrFile = Join-Path $evidenceDir "$seqStr-$CommandId-stderr.txt"

        $onStartedCallback = {
            # R2-08: Immediately when OS process start succeeds, transition mutation state
            if ($Mutating -eq $true) {
                $engineState.MutationState = 'UNKNOWN'
                $engineState.ProvenBranch = $null
                $engineState.ProvenHead = $null
            }
        }

        $execResult = Invoke-PalkaNativeProcess -Executable $Executable -Arguments $Arguments -Cwd $Cwd -StdoutPath $stdoutFile -StderrPath $stderrFile -CommandId $CommandId -Phase $Phase -OnStarted $onStartedCallback -TestPostStartHook $TestPostStartHook

        $rec = [PSCustomObject]@{
            sequence = $engineState.Sequence
            command_id = $CommandId
            phase = $Phase
            executable = $Executable
            arguments = $Arguments
            cwd = $Cwd
            mutating = $Mutating
            start_time_utc = $execResult.StartTimeUtc
            end_time_utc = $execResult.EndTimeUtc
            exit_code = $execResult.ExitCode
            stdout_path = $stdoutFile
            stderr_path = $stderrFile
        }

        # R3-02: Distinguish not launched from post-start engine/capture error
        if (-not $execResult.Launched) {
            $rec | Add-Member -NotePropertyName launch_error -NotePropertyValue $execResult.LaunchError
            & $writeCommandRecord $rec
            throw [PalkaEngineException]::new('LAUNCH_FAILURE', $CommandId, $Phase, "Process '$CommandId' failed to launch: $($execResult.LaunchError)")
        }

        if ($null -ne $execResult.EngineError) {
            $rec | Add-Member -NotePropertyName engine_error -NotePropertyValue $execResult.EngineError
            & $writeCommandRecord $rec
            throw [PalkaEngineException]::new('ENGINE_FAILURE', $CommandId, $Phase, "Process '$CommandId' engine/capture error: $($execResult.EngineError)")
        }

        & $writeCommandRecord $rec

        # Verify expectations if specified
        if ($null -ne $Expect) {
            if ($execResult.ExitCode -ne $Expect.exit_code) {
                throw [PalkaEngineException]::new('EXPECTATION_MISMATCH', $CommandId, $Phase, "Command '$CommandId' exit code mismatch: got $($execResult.ExitCode), expected $($Expect.exit_code)")
            }

            $cleanStdout = Normalize-PalkaStreamText $execResult.StdoutText
            $cleanStderr = Normalize-PalkaStreamText $execResult.StderrText

            $expProps = @($Expect.PSObject.Properties | ForEach-Object { $_.Name })
            if ('stdout_equals' -in $expProps) {
                if ($cleanStdout -ne $Expect.stdout_equals) {
                    throw [PalkaEngineException]::new('EXPECTATION_MISMATCH', $CommandId, $Phase, "Command '$CommandId' stdout mismatch: expected '$($Expect.stdout_equals)', got '$cleanStdout'")
                }
            }
            if ('stdout_empty' -in $expProps -and $Expect.stdout_empty -eq $true) {
                if ($execResult.StdoutText.Length -ne 0) {
                    throw [PalkaEngineException]::new('EXPECTATION_MISMATCH', $CommandId, $Phase, "Command '$CommandId' expected empty stdout, got '$cleanStdout'")
                }
            }
            if ('stderr_equals' -in $expProps) {
                if ($cleanStderr -ne $Expect.stderr_equals) {
                    throw [PalkaEngineException]::new('EXPECTATION_MISMATCH', $CommandId, $Phase, "Command '$CommandId' stderr mismatch: expected '$($Expect.stderr_equals)', got '$cleanStderr'")
                }
            }
            if ('stderr_empty' -in $expProps -and $Expect.stderr_empty -eq $true) {
                if ($execResult.StderrText.Length -ne 0) {
                    throw [PalkaEngineException]::new('EXPECTATION_MISMATCH', $CommandId, $Phase, "Command '$CommandId' expected empty stderr, got '$cleanStderr'")
                }
            }
        }

        return $execResult
    }

    try {
        # Phase 0: Read Manifest and Strict UTF-8 decode
        $failedPhase = 'MANIFEST_READ'
        if (-not (Test-Path -LiteralPath $ManifestPath -PathType Leaf)) {
            throw [PalkaEngineException]::new('ENGINE_FAILURE', "Manifest file does not exist: '$ManifestPath'")
        }

        $rawManifestBytes = [System.IO.File]::ReadAllBytes($ManifestPath)
        $utf8Strict = New-Object System.Text.UTF8Encoding($false, $true)
        $manifestJsonText = $utf8Strict.GetString($rawManifestBytes)

        try {
            $manifest = $manifestJsonText | ConvertFrom-Json
        }
        catch {
            throw [PalkaEngineException]::new('ENGINE_FAILURE', "Malformed JSON in manifest: $($_.Exception.Message)")
        }

        # Phase 1: Manifest Structural Validation
        $failedPhase = 'MANIFEST_VALIDATION'
        Test-PalkaManifestStructure -ManifestObject $manifest

        $operationId = $manifest.operation_id

        # Phase 1.5: Validate OutputRoot vs working_directory (R2-10: Windows case-insensitive safety)
        $failedPhase = 'OUTPUT_ROOT_VALIDATION'
        $canonWorkDir = [System.IO.Path]::GetFullPath($manifest.working_directory).TrimEnd([char]'\', [char]'/')
        $canonOutputRoot = [System.IO.Path]::GetFullPath($OutputRoot).TrimEnd([char]'\', [char]'/')

        if ([string]::Equals($canonOutputRoot, $canonWorkDir, [System.StringComparison]::OrdinalIgnoreCase) -or
            $canonOutputRoot.StartsWith($canonWorkDir + '\', [System.StringComparison]::OrdinalIgnoreCase) -or
            $canonOutputRoot.StartsWith($canonWorkDir + '/', [System.StringComparison]::OrdinalIgnoreCase)) {
            throw [PalkaEngineException]::new('POLICY_FAILURE', "OutputRoot ('$canonOutputRoot') must not be equal to or inside working_directory ('$canonWorkDir')")
        }

        # Create run directory and evidence directory outside repository
        $runDir = Join-Path (Join-Path $canonOutputRoot $operationId) $runId
        New-Item -ItemType Directory -Force -Path $runDir | Out-Null
        $evidenceDir = Join-Path $runDir 'evidence'
        New-Item -ItemType Directory -Force -Path $evidenceDir | Out-Null

        # Save byte-exact original manifest
        [System.IO.File]::WriteAllBytes((Join-Path $runDir 'manifest.json'), $rawManifestBytes)

        $commandsJournalPath = Join-Path $runDir 'commands.jsonl'
        $summaryPath = Join-Path $runDir 'summary.json'

        # Determine initial mutation state
        $hasMutatingAuthorizedCommand = $false
        if ($null -ne $manifest.authorized_commands) {
            foreach ($ac in $manifest.authorized_commands) {
                if ($ac.mutating -eq $true) {
                    $hasMutatingAuthorizedCommand = $true
                    break
                }
            }
        }
        if ($hasMutatingAuthorizedCommand) {
            $engineState.MutationState = 'NOT_APPLIED'
        }
        else {
            $engineState.MutationState = 'NONE'
        }

        # Phase 2: Built-in Local Identity Preflight
        $failedPhase = 'LOCAL_IDENTITY_PREFLIGHT'
        $workDir = $manifest.working_directory
        if (-not (Test-Path -LiteralPath $workDir -PathType Container)) {
            throw [PalkaEngineException]::new('ENGINE_FAILURE', "working_directory does not exist: '$workDir'")
        }

        # Builtin 1: toplevel
        $failedCommandId = 'builtin-preflight-toplevel'
        $topProc = & $executeJournaledProcess -CommandId 'builtin-preflight-toplevel' -Phase 'LOCAL_IDENTITY_PREFLIGHT' -Executable 'git' -Arguments @('rev-parse', '--show-toplevel') -Cwd $workDir -Mutating $false -Expect ([PSCustomObject]@{ exit_code = 0 })
        $canonGitDir = [System.IO.Path]::GetFullPath((Normalize-PalkaStreamText $topProc.StdoutText)).TrimEnd([char]'\', [char]'/')
        if (-not [string]::Equals($canonWorkDir, $canonGitDir, [System.StringComparison]::OrdinalIgnoreCase)) {
            throw [PalkaEngineException]::new('EXPECTATION_MISMATCH', 'builtin-preflight-toplevel', 'LOCAL_IDENTITY_PREFLIGHT', "working_directory ('$canonWorkDir') does not resolve to git root ('$canonGitDir')")
        }

        # Builtin 2: current branch
        $failedCommandId = 'builtin-preflight-branch'
        $branchProc = & $executeJournaledProcess -CommandId 'builtin-preflight-branch' -Phase 'LOCAL_IDENTITY_PREFLIGHT' -Executable 'git' -Arguments @('branch', '--show-current') -Cwd $workDir -Mutating $false -Expect ([PSCustomObject]@{ exit_code = 0 })
        $curBranch = (Normalize-PalkaStreamText $branchProc.StdoutText).Trim()
        if ($curBranch -ne $manifest.expected_start_branch) {
            throw [PalkaEngineException]::new('EXPECTATION_MISMATCH', 'builtin-preflight-branch', 'LOCAL_IDENTITY_PREFLIGHT', "Current branch ('$curBranch') does not match expected_start_branch ('$($manifest.expected_start_branch)')")
        }
        $engineState.ProvenBranch = $curBranch

        # Builtin 3: HEAD
        $failedCommandId = 'builtin-preflight-head'
        $headProc = & $executeJournaledProcess -CommandId 'builtin-preflight-head' -Phase 'LOCAL_IDENTITY_PREFLIGHT' -Executable 'git' -Arguments @('rev-parse', 'HEAD') -Cwd $workDir -Mutating $false -Expect ([PSCustomObject]@{ exit_code = 0 })
        $curHead = (Normalize-PalkaStreamText $headProc.StdoutText).Trim()
        if ($curHead -ne $manifest.expected_head) {
            throw [PalkaEngineException]::new('EXPECTATION_MISMATCH', 'builtin-preflight-head', 'LOCAL_IDENTITY_PREFLIGHT', "Current HEAD ('$curHead') does not match expected_head ('$($manifest.expected_head)')")
        }
        $engineState.ProvenHead = $curHead
        $failedCommandId = $null

        # Phase 3: Refresh Commands
        $failedPhase = 'REFRESH_COMMANDS'
        if ($null -ne $manifest.refresh_commands) {
            foreach ($rc in $manifest.refresh_commands) {
                $failedCommandId = $rc.id
                $cmdCwd = if ($null -ne $rc.cwd -and $rc.cwd.Length -gt 0) { $rc.cwd } else { $workDir }
                & $executeJournaledProcess -CommandId $rc.id -Phase 'REFRESH' -Executable $rc.executable -Arguments $rc.arguments -Cwd $cmdCwd -Mutating $false -Expect $rc.expect | Out-Null
            }
        }
        $failedCommandId = $null

        # Phase 4: Expected Remote Refs Verification (R2-14: collision-proof safe command IDs)
        $failedPhase = 'REMOTE_REFS_VERIFICATION'
        if ($null -ne $manifest.expected_remote_refs) {
            $refIndex = 0
            foreach ($prop in $manifest.expected_remote_refs.PSObject.Properties) {
                $refIndex++
                $refKey = $prop.Name # e.g. origin/main or origin/foo-bar
                $expectedSha = $prop.Value
                $branchName = $refKey.Substring('origin/'.Length)
                $safeSanitized = $refKey -replace '[^A-Za-z0-9_-]', '-'
                $safeRefId = "builtin-ref-{0:D3}-{1}" -f $refIndex, $safeSanitized
                $failedCommandId = $safeRefId

                if ($expectedSha -eq 'ABSENT') {
                    $lsProc = & $executeJournaledProcess -CommandId $safeRefId -Phase 'REMOTE_REFS_VERIFICATION' -Executable 'git' -Arguments @('ls-remote', '--heads', 'origin', "refs/heads/$branchName") -Cwd $workDir -Mutating $false -Expect ([PSCustomObject]@{ exit_code = 0 })
                    if ($lsProc.StdoutText.Trim().Length -gt 0) {
                        throw [PalkaEngineException]::new('EXPECTATION_MISMATCH', $safeRefId, 'REMOTE_REFS_VERIFICATION', "Remote branch '$refKey' expected ABSENT but was found on remote: $($lsProc.StdoutText.Trim())")
                    }
                }
                else {
                    $refProc = & $executeJournaledProcess -CommandId $safeRefId -Phase 'REMOTE_REFS_VERIFICATION' -Executable 'git' -Arguments @('rev-parse', $refKey) -Cwd $workDir -Mutating $false -Expect ([PSCustomObject]@{ exit_code = 0 })
                    $actualSha = (Normalize-PalkaStreamText $refProc.StdoutText).Trim()
                    if ($actualSha -ne $expectedSha) {
                        throw [PalkaEngineException]::new('EXPECTATION_MISMATCH', $safeRefId, 'REMOTE_REFS_VERIFICATION', "Remote ref '$refKey' SHA mismatch: expected '$expectedSha', got '$actualSha'")
                    }
                }
            }
        }
        $failedCommandId = $null

        # Phase 5: Required Preconditions
        $failedPhase = 'PRECONDITIONS'
        if ($null -ne $manifest.required_preconditions) {
            foreach ($pc in $manifest.required_preconditions) {
                $failedCommandId = $pc.id
                $cmdCwd = if ($null -ne $pc.cwd -and $pc.cwd.Length -gt 0) { $pc.cwd } else { $workDir }
                & $executeJournaledProcess -CommandId $pc.id -Phase 'PRECONDITION' -Executable $pc.executable -Arguments $pc.arguments -Cwd $cmdCwd -Mutating $false -Expect $pc.expect | Out-Null
            }
        }
        $failedCommandId = $null

        # Phase 6: Already Satisfied Checks (R2-02, R2-03)
        $failedPhase = 'ALREADY_SATISFIED_EVALUATION'
        $isAlreadySatisfied = $false
        if ($null -ne $manifest.already_satisfied_checks -and $manifest.already_satisfied_checks.Count -gt 0) {
            $allChecksPassed = $true
            foreach ($asc in $manifest.already_satisfied_checks) {
                $failedCommandId = $asc.id
                $cmdCwd = if ($null -ne $asc.cwd -and $asc.cwd.Length -gt 0) { $asc.cwd } else { $workDir }

                try {
                    & $executeJournaledProcess -CommandId $asc.id -Phase 'ALREADY_SATISFIED_CHECK' -Executable $asc.executable -Arguments $asc.arguments -Cwd $cmdCwd -Mutating $false -Expect $asc.expect | Out-Null
                }
                catch [PalkaEngineException] {
                    # R2-02: Only EXPECTATION_MISMATCH is allowed to continue to action phase
                    if ($_.Exception.FailureKind -eq 'EXPECTATION_MISMATCH') {
                        $allChecksPassed = $false
                        break
                    }
                    else {
                        throw $_
                    }
                }
                catch {
                    throw $_
                }
            }
            $failedCommandId = $null

            if ($allChecksPassed) {
                # R2-03: Additional journaled read-only branch proof before returning ALREADY_SATISFIED
                $failedCommandId = 'builtin-already-satisfied-branch'
                $asBranchProc = & $executeJournaledProcess -CommandId 'builtin-already-satisfied-branch' -Phase 'ALREADY_SATISFIED_EVALUATION' -Executable 'git' -Arguments @('branch', '--show-current') -Cwd $workDir -Mutating $false -Expect ([PSCustomObject]@{ exit_code = 0 })
                $asBranch = (Normalize-PalkaStreamText $asBranchProc.StdoutText).Trim()
                $failedCommandId = $null

                if ($asBranch -eq $manifest.target_branch) {
                    $isAlreadySatisfied = $true
                    $engineState.ProvenBranch = $asBranch
                }
                else {
                    # Branch mismatch: complete intended postcondition is not already satisfied; proceed to action phase
                    $isAlreadySatisfied = $false
                }
            }
        }

        if ($isAlreadySatisfied) {
            $result = 'ALREADY_SATISFIED'
            $engineState.MutationState = 'NONE'
            $failedPhase = $null
        }
        else {
            # Builtin 4: Scope before
            $failedPhase = 'SCOPE_VERIFICATION'
            $failedCommandId = 'builtin-scope-before'
            $statusBeforeProc = & $executeJournaledProcess -CommandId 'builtin-scope-before' -Phase 'SCOPE_VERIFICATION' -Executable 'git' -Arguments @('status', '--porcelain=v1', '-z', '--untracked-files=all') -Cwd $workDir -Mutating $false -Expect ([PSCustomObject]@{ exit_code = 0 })
            $failedCommandId = $null

            # Phase 7: Authorized Commands
            $failedPhase = 'AUTHORIZED_COMMANDS'
            if ($null -ne $manifest.authorized_commands) {
                foreach ($ac in $manifest.authorized_commands) {
                    $failedCommandId = $ac.id
                    $cmdCwd = if ($null -ne $ac.cwd -and $ac.cwd.Length -gt 0) { $ac.cwd } else { $workDir }
                    & $executeJournaledProcess -CommandId $ac.id -Phase 'ACTION' -Executable $ac.executable -Arguments $ac.arguments -Cwd $cmdCwd -Mutating ([bool]$ac.mutating) -Expect $ac.expect | Out-Null
                }
            }
            $failedCommandId = $null

            # Phase 8: Local Scope Verification (R2-09: Robust NUL-separated porcelain -z parser)
            $failedPhase = 'SCOPE_VERIFICATION'
            $failedCommandId = 'builtin-scope-after'
            $statusAfterProc = & $executeJournaledProcess -CommandId 'builtin-scope-after' -Phase 'SCOPE_VERIFICATION' -Executable 'git' -Arguments @('status', '--porcelain=v1', '-z', '--untracked-files=all') -Cwd $workDir -Mutating $false -Expect ([PSCustomObject]@{ exit_code = 0 })
            $failedCommandId = $null

            $changedPaths = Get-PalkaPorcelainZPaths -RawStatusText $statusAfterProc.StdoutText

            foreach ($cf in $changedPaths) {
                $cfNorm = $cf.Replace('\', '/')

                # Check forbidden
                if ($null -ne $manifest.forbidden_paths) {
                    foreach ($fp in $manifest.forbidden_paths) {
                        if (Test-PalkaGlobMatch -Path $cfNorm -Pattern $fp) {
                            throw [PalkaEngineException]::new('POLICY_FAILURE', "Scope violation: changed file '$cfNorm' matches forbidden pattern '$fp'")
                        }
                    }
                }

                # Check authorized
                $isAuth = $false
                if ($null -ne $manifest.authorized_paths) {
                    foreach ($ap in $manifest.authorized_paths) {
                        if (Test-PalkaGlobMatch -Path $cfNorm -Pattern $ap) {
                            $isAuth = $true
                            break
                        }
                    }
                }
                if (-not $isAuth) {
                    throw [PalkaEngineException]::new('POLICY_FAILURE', "Scope violation: changed file '$cfNorm' does not match any authorized_paths pattern")
                }
            }

            # Phase 9: Required Postconditions
            $failedPhase = 'POSTCONDITIONS'
            if ($null -ne $manifest.required_postconditions) {
                foreach ($postc in $manifest.required_postconditions) {
                    $failedCommandId = $postc.id
                    $cmdCwd = if ($null -ne $postc.cwd -and $postc.cwd.Length -gt 0) { $postc.cwd } else { $workDir }
                    & $executeJournaledProcess -CommandId $postc.id -Phase 'POSTCONDITION' -Executable $postc.executable -Arguments $postc.arguments -Cwd $cmdCwd -Mutating $false -Expect $postc.expect | Out-Null
                }
            }
            $failedCommandId = $null

            # Phase 9.5: Final Target Branch & Head Proof
            $failedPhase = 'FINAL_PROOF'
            $failedCommandId = 'builtin-final-branch'
            $finalBranchProc = & $executeJournaledProcess -CommandId 'builtin-final-branch' -Phase 'FINAL_PROOF' -Executable 'git' -Arguments @('branch', '--show-current') -Cwd $workDir -Mutating $false -Expect ([PSCustomObject]@{ exit_code = 0 })
            $finalBranchVal = (Normalize-PalkaStreamText $finalBranchProc.StdoutText).Trim()
            if ($finalBranchVal -ne $manifest.target_branch) {
                throw [PalkaEngineException]::new('EXPECTATION_MISMATCH', 'builtin-final-branch', 'FINAL_PROOF', "Final branch ('$finalBranchVal') does not match target_branch ('$($manifest.target_branch)')")
            }
            $engineState.ProvenBranch = $finalBranchVal

            $failedCommandId = 'builtin-final-head'
            $finalHeadProc = & $executeJournaledProcess -CommandId 'builtin-final-head' -Phase 'FINAL_PROOF' -Executable 'git' -Arguments @('rev-parse', 'HEAD') -Cwd $workDir -Mutating $false -Expect ([PSCustomObject]@{ exit_code = 0 })
            $finalHeadVal = (Normalize-PalkaStreamText $finalHeadProc.StdoutText).Trim()
            $engineState.ProvenHead = $finalHeadVal
            $failedCommandId = $null

            # All phases completed successfully
            $result = 'COMPLETED'
            if ($hasMutatingAuthorizedCommand) {
                $engineState.MutationState = 'APPLIED'
            }
            else {
                $engineState.MutationState = 'NONE'
            }
            $failedPhase = $null
        }
    }
    catch [PalkaEngineException] {
        $result = 'STOPPED'
        $failureReason = $_.Exception.Message
        if (-not [string]::IsNullOrEmpty($_.Exception.Phase)) {
            $failedPhase = $_.Exception.Phase
        }
        if (-not [string]::IsNullOrEmpty($_.Exception.CommandId)) {
            $failedCommandId = $_.Exception.CommandId
        }
    }
    catch {
        $result = 'STOPPED'
        $failureReason = $_.Exception.Message
    }

    $endedAt = [DateTime]::UtcNow
    $mutationState = $engineState.MutationState

    # R2-01: final_branch and final_head derived ONLY from already journaled facts, NO direct native process queries
    $finalBranch = $engineState.ProvenBranch
    $finalHead = $engineState.ProvenHead

    # Summary object
    $summaryObj = [PSCustomObject]@{
        schema = 'palka.operation-summary/v1'
        operation_id = $operationId
        run_id = $runId
        result = $result
        mutation_state = $mutationState
        started_at_utc = $startedAt.ToString('o')
        ended_at_utc = $endedAt.ToString('o')
        repository = if ($null -ne $manifest) { $manifest.repository } else { $null }
        working_directory = if ($null -ne $manifest) { $manifest.working_directory } else { $null }
        stage = if ($null -ne $manifest) { $manifest.stage } else { $null }
        expected_start_branch = if ($null -ne $manifest) { $manifest.expected_start_branch } else { $null }
        target_branch = if ($null -ne $manifest) { $manifest.target_branch } else { $null }
        expected_head = if ($null -ne $manifest) { $manifest.expected_head } else { $null }
        expected_base = if ($null -ne $manifest) { $manifest.expected_base } else { $null }
        final_branch = $finalBranch
        final_head = $finalHead
        failed_phase = $failedPhase
        failed_command_id = $failedCommandId
        reason = $failureReason
        command_count = $executedRecords.Count
        run_directory = $runDir
    }

    if ($null -ne $summaryPath) {
        try {
            $summaryJson = $summaryObj | ConvertTo-Json -Depth 5
            [System.IO.File]::WriteAllText($summaryPath, $summaryJson, [System.Text.UTF8Encoding]::new($false))
        }
        catch {
            # R2-13: If summary cannot be written, result remains STOPPED
            $summaryObj.result = 'STOPPED'
            $summaryObj.reason = "Failed to write summary.json: $($_.Exception.Message)"
        }
    }

    if ($PassThru) {
        return $summaryObj
    }
    return $summaryPath
}

Export-ModuleMember -Function @(
    'Invoke-PalkaEngine',
    'Test-PalkaManifestStructure',
    'Format-PalkaProcessArgument',
    'Test-PalkaDangerousPolicy',
    'Test-PalkaRefreshPolicy',
    'Get-PalkaPorcelainZPaths'
)
