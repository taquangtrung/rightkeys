# rightkeys Windows install helper, driven by the Makefile.
#
# Actions:
#   install   copy the built exe, seed the config, add to PATH, add a Startup shortcut
#   config    seed the user config only
#   uninstall reverse an install (the user config is left in place)
#
# Run via the Makefile (`make install` / `make uninstall` / `make install-config`)
# or directly:  powershell -File scripts/windows-setup.ps1 -Action install
[CmdletBinding()]
param(
    [ValidateSet('install', 'config', 'uninstall')]
    [string]$Action = 'install',
    [string]$Prefix = (Join-Path $env:LOCALAPPDATA 'Programs\rightkeys')
)

$ErrorActionPreference = 'Stop'

$RepoRoot  = Split-Path -Parent $PSScriptRoot
$ExeSource = Join-Path $RepoRoot 'target\release\rightkeys.exe'
$ExeTarget = Join-Path $Prefix 'rightkeys.exe'
$CfgSource = Join-Path $RepoRoot 'config.example.kdl'
$CfgDir    = Join-Path $env:APPDATA 'rightkeys'
$CfgTarget = Join-Path $CfgDir 'config.kdl'
$Startup   = [Environment]::GetFolderPath('Startup')
$Shortcut  = Join-Path $Startup 'rightkeys.lnk'
$Programs  = [Environment]::GetFolderPath('Programs')
$MenuLink  = Join-Path $Programs 'RightKeys.lnk'

function New-Shortcut($Path) {
    $shell = New-Object -ComObject WScript.Shell
    $lnk = $shell.CreateShortcut($Path)
    $lnk.TargetPath = $ExeTarget
    $lnk.WorkingDirectory = $Prefix
    $lnk.Description = 'RightKeys key remapper'
    $lnk.Save()
}

function Install-Binary {
    if (-not (Test-Path $ExeSource)) {
        throw "$ExeSource not found. Run 'make build' first."
    }
    New-Item -ItemType Directory -Force -Path $Prefix | Out-Null
    Copy-Item -Force $ExeSource $ExeTarget
    Write-Host "installed rightkeys.exe to $Prefix"
}

function Install-Config {
    if (-not (Test-Path $CfgSource)) {
        throw "$CfgSource not found."
    }
    New-Item -ItemType Directory -Force -Path $CfgDir | Out-Null
    if (Test-Path $CfgTarget) {
        Write-Host "config exists; not overwriting $CfgTarget"
    }
    else {
        Copy-Item $CfgSource $CfgTarget
        Write-Host "installed config to $CfgTarget"
    }
}

function Add-ToUserPath {
    $current = [Environment]::GetEnvironmentVariable('Path', 'User')
    $entries = @()
    if ($current) { $entries = $current -split ';' | Where-Object { $_ -ne '' } }
    if ($entries -contains $Prefix) {
        Write-Host "already on user PATH: $Prefix"
        return
    }
    $entries += $Prefix
    [Environment]::SetEnvironmentVariable('Path', ($entries -join ';'), 'User')
    Write-Host "added to user PATH (open a new terminal to pick it up): $Prefix"
}

function Install-Shortcuts {
    New-Shortcut $MenuLink
    Write-Host "added Start Menu entry: $MenuLink"
    New-Shortcut $Shortcut
    Write-Host "added Startup shortcut: $Shortcut"
}

function Uninstall-Shortcuts {
    foreach ($s in @($MenuLink, $Shortcut)) {
        if (Test-Path $s) {
            Remove-Item -Force $s
            Write-Host "removed shortcut: $s"
        }
    }
}

function Remove-FromUserPath {
    $current = [Environment]::GetEnvironmentVariable('Path', 'User')
    if (-not $current) { return }
    $entries = $current -split ';' | Where-Object { $_ -ne '' -and $_ -ne $Prefix }
    [Environment]::SetEnvironmentVariable('Path', ($entries -join ';'), 'User')
    Write-Host "removed from user PATH: $Prefix"
}

function Uninstall-Binary {
    if (Test-Path $ExeTarget) {
        Remove-Item -Force $ExeTarget
        Write-Host "removed $ExeTarget"
    }
    if ((Test-Path $Prefix) -and -not (Get-ChildItem -Force $Prefix)) {
        Remove-Item -Force $Prefix
    }
}

switch ($Action) {
    'install' {
        Install-Binary
        Install-Config
        Add-ToUserPath
        Install-Shortcuts
        Write-Host 'rightkeys installed for the current user.'
    }
    'config' {
        Install-Config
    }
    'uninstall' {
        Uninstall-Shortcuts
        Remove-FromUserPath
        Uninstall-Binary
        Write-Host "rightkeys uninstalled (user config left at $CfgTarget)."
    }
}
