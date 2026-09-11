<#
.SYNOPSIS
Installs quix and quixd, puts them on PATH, and runs the daemon as a Windows service.

.EXAMPLE
    .\install.ps1              # from the latest GitHub release
    .\install.ps1 -Local       # from .\target\release (builds if needed)
    .\install.ps1 -Uninstall

Run from an elevated PowerShell: the service and the machine PATH both need it.
#>
[CmdletBinding()]
param(
    [switch]$Local,
    [switch]$Uninstall,
    [string]$InstallDir = "$env:ProgramFiles\quix",
    [string]$Repo = 'quixvpn/quix'
)

$ErrorActionPreference = 'Stop'
$Service = 'quixd'

function Info($msg) { Write-Host "==> $msg" }

$identity = [Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
if (-not $identity.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'needs an elevated PowerShell (installs a service and edits the machine PATH)'
}

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
    Set-ItemProperty `
        -Path "HKLM:\SYSTEM\CurrentControlSet\Services\$Service" `
        -Name Environment `
        -Type MultiString `
        -Value @(
            "QUIX_KEY_PATH=$stateDir\key",
            "QUIX_NETWORK_PATH=$stateDir\network.json",
            "QUIX_SETTINGS_PATH=$stateDir\settings.json"
        )

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
