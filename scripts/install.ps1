<#
.SYNOPSIS
Installs quix and quixd, puts them on PATH, and runs the daemon as a Windows service.

.EXAMPLE
    .\install.ps1              # from the latest GitHub release
    .\install.ps1 -Local       # from .\target\release (builds if needed)
    .\install.ps1 -Uninstall

Installing a service and editing the machine PATH both need Administrator. Run
this from an ordinary PowerShell and it asks for it: no elevated terminal needed.
#>
[CmdletBinding()]
param(
    [switch]$Local,
    [switch]$Uninstall,
    [string]$InstallDir = "$env:ProgramFiles\quix",
    [string]$Repo = 'quixvpn/quix',
    # Set when this script re-runs itself elevated: where to transcribe output
    # so the shell that asked for elevation can show it. Plumbing between two
    # copies of this script, not something to pass by hand.
    [string]$LogTo,
    # The account to make the operator, as a SID. Carried across the elevation
    # boundary because the elevated process cannot tell who asked for it:
    # Windows has no $SUDO_USER, and if elevation was approved with someone
    # else's credentials then GetCurrent() there is the approver, not the user.
    [string]$OperatorSid
)

$ErrorActionPreference = 'Stop'
$Service = 'quixd'

function Info($msg) { Write-Host "==> $msg" }

function Test-Administrator {
    $identity = [Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
    $identity.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

<#
.SYNOPSIS
Re-runs this script as Administrator, which is what raises the UAC prompt.

.DESCRIPTION
An elevated process cannot write to this console, and Start-Process refuses to
redirect output while it is also elevating, so the child transcribes itself to a
file that gets printed here once it exits. The child's window is left visible on
purpose, unlike the CLI's: an install downloads release assets and waits for the
daemon to come up, and a hidden window would look like a hang.
#>
function Invoke-SelfElevated {
    $script = $PSCommandPath
    if (-not $script) {
        # Started from a pipe (`irm ... | iex`), so there is no file to re-run
        # and no reliable way to recover the running source. Fetch it again —
        # the same source that is already executing.
        $script = Join-Path ([IO.Path]::GetTempPath()) "quix-install-$([Guid]::NewGuid()).ps1"
        $url = "https://raw.githubusercontent.com/$Repo/master/scripts/install.ps1"
        Info 'fetching the installer so it can be re-run as Administrator'
        Invoke-WebRequest -Uri $url -OutFile $script -UseBasicParsing
    }

    $log = Join-Path ([IO.Path]::GetTempPath()) "quix-install-$([Guid]::NewGuid()).log"

    # Each element is quoted here because Start-Process joins this array with
    # spaces and quotes nothing itself, so "C:\Program Files\quix" would
    # otherwise arrive as two arguments.
    # Captured here, on this side of the prompt, because this is the only place
    # that knows who is installing. This is the $SUDO_USER of the Windows side.
    $sid = ([Security.Principal.WindowsIdentity]::GetCurrent()).User.Value

    $argv = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', "`"$script`"",
              '-LogTo', "`"$log`"", '-OperatorSid', "`"$sid`"")
    foreach ($entry in $PSBoundParameters.GetEnumerator()) {
        if ($entry.Key -in 'LogTo', 'OperatorSid') { continue }
        if ($entry.Value -is [switch]) {
            if ($entry.Value.IsPresent) { $argv += "-$($entry.Key)" }
        }
        else {
            $argv += "-$($entry.Key)"
            $argv += "`"$($entry.Value)`""
        }
    }

    try {
        $child = Start-Process -FilePath 'powershell.exe' -ArgumentList $argv -Verb RunAs -Wait -PassThru
    }
    catch {
        # Declining the prompt is a decision, not a crash, so it gets a plain
        # sentence rather than a PowerShell error record.
        Write-Host 'elevation was declined - this installer needs Administrator (it installs a service and edits the machine PATH)'
        exit 1
    }

    Show-Transcript $log
    Remove-Item $log -Force -ErrorAction SilentlyContinue
    exit $child.ExitCode
}

# Prints what the elevated run produced, without the transcript's own banner.
function Show-Transcript($path) {
    if (-not (Test-Path $path)) { return }

    $lines = @(Get-Content -LiteralPath $path)
    $banners = @(0..($lines.Count - 1) | Where-Object { $lines[$_] -match '^\*{10,}$' })

    # Header, then what the script printed, then footer. Anything else means the
    # format is not what we expect, so show all of it rather than guess.
    if ($banners.Count -ge 3 -and ($banners[2] - 1) -ge ($banners[1] + 1)) {
        $lines[($banners[1] + 1)..($banners[2] - 1)] | ForEach-Object { Write-Host $_ }
    }
    else {
        $lines | ForEach-Object { Write-Host $_ }
    }
}

<#
.SYNOPSIS
Checks a SID names a real user account, and returns its display name.

.DESCRIPTION
Two things are being refused. A malformed or unresolvable SID would be stored
as an operator that can never match anyone, which looks like it worked and
grants nothing. A well-known or group SID — Everyone, Authenticated Users,
BUILTIN\Administrators — is far too broad to hand this authority to; those all
fail the account-SID shape below, which only local, domain and Entra accounts
carry.
#>
function Resolve-OperatorAccount($sid) {
    if ($sid -notmatch '^S-1-(5-21|12-1)-[0-9]+(-[0-9]+)*$') {
        throw "refusing to make $sid the operator: not a user account SID"
    }
    try {
        $account = (New-Object Security.Principal.SecurityIdentifier($sid)).Translate(
            [Security.Principal.NTAccount]).Value
    }
    catch {
        throw "refusing to make $sid the operator: it does not resolve to an account"
    }
    return $account
}

if (-not (Test-Administrator)) { Invoke-SelfElevated }

# Run elevated directly and nobody passed one, so the person at the keyboard is
# the installing user.
if (-not $OperatorSid) {
    $OperatorSid = ([Security.Principal.WindowsIdentity]::GetCurrent()).User.Value
}

# Elevated now. Transcribe so the shell that asked for elevation can show this;
# PowerShell closes the transcript when the process exits, including on the
# early return from -Uninstall.
if ($LogTo) { Start-Transcript -Path $LogTo | Out-Null }

function Remove-QuixService {
    if (Get-Service -Name $Service -ErrorAction SilentlyContinue) {
        Info "stopping $Service"
        Stop-Service -Name $Service -Force -ErrorAction SilentlyContinue
        # sc.exe delete rather than Remove-Service, which needs PowerShell 6+.
        & sc.exe delete $Service | Out-Null
        Start-Sleep -Seconds 1
    }
}

if ($Uninstall) {
    Remove-QuixService
    if (Test-Path $InstallDir) { Remove-Item -Recurse -Force $InstallDir }

    $machinePath = [Environment]::GetEnvironmentVariable('Path', 'Machine')
    $trimmed = ($machinePath -split ';' | Where-Object { $_ -and $_ -ne $InstallDir }) -join ';'
    [Environment]::SetEnvironmentVariable('Path', $trimmed, 'Machine')

    Info "removed. identity and roster kept in $(Join-Path $env:ProgramData 'quix')"
    return
}

$stage = Join-Path ([IO.Path]::GetTempPath()) ("quix-" + [Guid]::NewGuid())
New-Item -ItemType Directory -Path $stage | Out-Null

try {
    if ($Local) {
        $root = Split-Path -Parent $PSScriptRoot
        if (-not (Test-Path "$root\target\release\quixd.exe")) {
            Info 'building release binaries'
            Push-Location $root
            try { cargo build --release --workspace } finally { Pop-Location }
        }
        Copy-Item "$root\target\release\quix.exe", "$root\target\release\quixd.exe" $stage
    }
    else {
        $asset = 'quix-windows-x86_64.zip'
        $base = "https://github.com/$Repo/releases/latest/download"

        Info "downloading $asset from $Repo"
        Invoke-WebRequest -Uri "$base/$asset" -OutFile "$stage\$asset" -UseBasicParsing
        Invoke-WebRequest -Uri "$base/$asset.sha256" -OutFile "$stage\$asset.sha256" -UseBasicParsing

        Info 'verifying checksum'
        $expected = ((Get-Content "$stage\$asset.sha256") -split '\s+')[0]
        $actual = (Get-FileHash "$stage\$asset" -Algorithm SHA256).Hash.ToLower()
        if ($expected -ne $actual) { throw "checksum mismatch - refusing to install" }

        Expand-Archive -Path "$stage\$asset" -DestinationPath $stage -Force
        Get-ChildItem -Path $stage -Recurse -Filter '*.exe' | Copy-Item -Destination $stage -ErrorAction SilentlyContinue
    }

    foreach ($exe in 'quix.exe', 'quixd.exe') {
        if (-not (Test-Path (Join-Path $stage $exe))) { throw "$exe missing from the package" }
    }

    # The service holds quixd.exe open, so it has to go before the copy.
    Remove-QuixService

    Info "installing to $InstallDir"
    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
    Copy-Item (Join-Path $stage 'quix.exe'), (Join-Path $stage 'quixd.exe') $InstallDir -Force

    $machinePath = [Environment]::GetEnvironmentVariable('Path', 'Machine')
    if (($machinePath -split ';') -notcontains $InstallDir) {
        Info 'adding to the machine PATH'
        [Environment]::SetEnvironmentVariable('Path', "$machinePath;$InstallDir", 'Machine')
        $env:Path = "$env:Path;$InstallDir"
    }

    Info "registering the $Service service"
    # LocalSystem: creating the Wintun adapter and installing routes needs it.
    New-Service -Name $Service `
        -BinaryPathName "`"$InstallDir\quixd.exe`"" `
        -DisplayName 'Quix P2P mesh VPN' `
        -Description 'Runs the quix mesh VPN data plane and local control socket.' `
        -StartupType Automatic | Out-Null

    # LocalSystem's profile directory is buried under system32; keep state in
    # ProgramData instead, mirroring what the systemd unit does with /var/lib.
    $stateDir = Join-Path $env:ProgramData 'quix'
    New-Item -ItemType Directory -Path $stateDir -Force | Out-Null

    # Anything created under ProgramData inherits read access for every local
    # user. The identity key lives here, and it is the node's whole identity on
    # the mesh, so the inherited list is replaced rather than added to. This is
    # what StateDirectoryMode=0700 does for the systemd unit.
    Info 'restricting access to the state directory'
    $acl = Get-Acl -Path $stateDir
    # $true drops inheritance, $false keeps none of what was inherited.
    $acl.SetAccessRuleProtection($true, $false)
    foreach ($rule in @($acl.Access)) { [void]$acl.RemoveAccessRule($rule) }
    foreach ($account in 'NT AUTHORITY\SYSTEM', 'BUILTIN\Administrators') {
        $acl.AddAccessRule((New-Object Security.AccessControl.FileSystemAccessRule(
            $account, 'FullControl', 'ContainerInherit,ObjectInherit', 'None', 'Allow')))
    }
    Set-Acl -Path $stateDir -AclObject $acl

    # An upgrade inherits nothing: files already there keep the permissive list
    # they were created with, so they are reset to the directory's.
    foreach ($existing in Get-ChildItem -Path $stateDir -Force -ErrorAction SilentlyContinue) {
        $childAcl = Get-Acl -Path $existing.FullName
        $childAcl.SetAccessRuleProtection($false, $false)
        foreach ($rule in @($childAcl.Access | Where-Object { -not $_.IsInherited })) {
            [void]$childAcl.RemoveAccessRule($rule)
        }
        Set-Acl -Path $existing.FullName -AclObject $childAcl
    }
    Set-ItemProperty `
        -Path "HKLM:\SYSTEM\CurrentControlSet\Services\$Service" `
        -Name Environment `
        -Type MultiString `
        -Value @(
            "QUIX_KEY_PATH=$stateDir\key",
            "QUIX_NETWORK_PATH=$stateDir\network.json",
            "QUIX_SETTINGS_PATH=$stateDir\settings.json",
            # A Windows service has no console, so without this every
            # diagnostic the daemon prints is lost.
            "QUIX_LOG_PATH=$stateDir\quixd.log"
        )

    # Whoever installed it is the obvious operator: they just installed it, and
    # without this every membership command would need a UAC prompt from here
    # on. Same default as the Linux installer's set-operator on $SUDO_USER.
    #
    # Written before the service starts, because the daemon reads its settings
    # once at startup.
    $operatorAccount = Resolve-OperatorAccount $OperatorSid
    Info "making $operatorAccount the operator"

    $settingsPath = Join-Path $stateDir 'settings.json'
    $settings = if (Test-Path $settingsPath) {
        Get-Content $settingsPath -Raw | ConvertFrom-Json
    }
    else {
        New-Object PSObject
    }
    # Add-Member -Force so an existing operator is replaced rather than doubled.
    $settings | Add-Member -NotePropertyName operator_sid -NotePropertyValue $OperatorSid -Force
    $settings | Add-Member -NotePropertyName operator_name -NotePropertyValue $operatorAccount -Force

    # WriteAllText with an explicit no-BOM encoding, NOT Set-Content -Encoding
    # utf8: in Windows PowerShell that means utf8 *with* a byte order mark, and
    # those three bytes are a syntax error to the JSON parser on the other side.
    # The daemon tolerates one now, but there is no reason to write one.
    [IO.File]::WriteAllText(
        $settingsPath,
        ($settings | ConvertTo-Json),
        (New-Object Text.UTF8Encoding($false)))

    # Come back automatically after a crash, matching Restart=on-failure on Linux.
    & sc.exe failure $Service reset= 86400 actions= restart/5000/restart/5000/restart/5000 | Out-Null

    Start-Service -Name $Service

    # The SCM reports Running as soon as the service reports it, which is
    # before the daemon has picked a relay (up to 10s), brought the TUN up and
    # opened its pipe. Wait for the pipe itself, which is the last thing it does.
    Info 'waiting for the daemon to come up'
    $ready = $false
    foreach ($attempt in 1..40) {
        $state = (Get-Service -Name $Service).Status
        if ($state -ne 'Running' -and $state -ne 'StartPending') {
            Write-Error "the service stopped while starting (state: $state). Check Event Viewer -> Windows Logs -> Application."
            exit 1
        }
        if (Test-Path '\\.\pipe\quix-daemon') { $ready = $true; break }
        Start-Sleep -Milliseconds 500
    }
    if (-not $ready) {
        Write-Error 'the daemon did not open its control pipe within 20s. Check Event Viewer -> Windows Logs -> Application.'
        exit 1
    }

    Write-Host ''
    Info 'installed'
    & "$InstallDir\quix.exe" status
    Write-Host ''
    Write-Host '  quix status          see this node and its peers'
    Write-Host '  quix create <name>   start a network'
    Write-Host '  quix join <code>     join one'
    Write-Host ''
    Write-Host 'Open a new terminal to pick up the PATH change.'
}
finally {
    Remove-Item -Recurse -Force $stage -ErrorAction SilentlyContinue
}
