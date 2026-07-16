<#
.SYNOPSIS
Installs luchta and bundled workers from GitHub release archives.

.DESCRIPTION
Downloads the latest or requested luchta release archive for current Windows
architecture, extracts all binaries into dedicated install directory, and
prints PATH instructions.

.PARAMETER Version
Plain version to install, for example 0.1.13. Overrides latest lookup.

.PARAMETER Dir
Install directory. Defaults to $env:USERPROFILE\.luchta\bin.

.EXAMPLE
./install.ps1

.EXAMPLE
./install.ps1 -Version 0.1.13 -Dir "$env:USERPROFILE\bin\luchta"
#>
[CmdletBinding()]
param(
    [Parameter()]
    [string]$Version = $env:LUCHTA_VERSION,

    [Parameter()]
    [string]$Dir = $(if ($env:LUCHTA_INSTALL_DIR) { $env:LUCHTA_INSTALL_DIR } else { Join-Path $env:USERPROFILE '.luchta\bin' })
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$Script:GitHubRepo = 'dobesv/luchta'
$Script:LatestReleaseApi = "https://api.github.com/repos/$Script:GitHubRepo/releases/latest"

function Fail([string]$Message) {
    throw $Message
}

function Get-AuthHeaders {
    $headers = @{}
    if ($env:GITHUB_TOKEN) {
        $headers['Authorization'] = "Bearer $($env:GITHUB_TOKEN)"
    }
    $headers
}

function Normalize-Version {
    param([Parameter(Mandatory)] [string]$InputVersion)

    if ($InputVersion.StartsWith('v')) {
        return $InputVersion.Substring(1)
    }

    return $InputVersion
}

function Get-ReleaseVersion {
    param([string]$RequestedVersion)

    if ($RequestedVersion) {
        return (Normalize-Version -InputVersion $RequestedVersion)
    }

    try {
        $response = Invoke-RestMethod -Uri $Script:LatestReleaseApi -Headers (Get-AuthHeaders)
    } catch {
        Fail "Failed to query GitHub latest release API. $($_.Exception.Message)"
    }

    if (-not $response.tag_name) {
        Fail 'GitHub API response did not contain tag_name.'
    }

    if ($response.tag_name -notlike 'luchta/v*') {
        Fail "Unexpected latest release tag format: $($response.tag_name)"
    }

    return (Normalize-Version -InputVersion $response.tag_name.Substring('luchta/v'.Length))
}

function Get-TargetTriple {
    $arch = $null

    try {
        $arch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
    } catch {
        $arch = $env:PROCESSOR_ARCHITECTURE
    }

    switch -Regex ($arch) {
        '^X64$|^AMD64$' { return 'x86_64-pc-windows-msvc' }
        '^Arm64$|^ARM64$' { return 'aarch64-pc-windows-msvc' }
        '^X86$|^x86$|^i386$' { return 'i686-pc-windows-msvc' }
        default { Fail "Unsupported Windows architecture: $arch" }
    }
}

function Get-ArchiveUrl {
    param(
        [Parameter(Mandatory)] [string]$ResolvedVersion,
        [Parameter(Mandatory)] [string]$TargetTriple
    )

    "https://github.com/$Script:GitHubRepo/releases/download/luchta/v$ResolvedVersion/luchta-v$ResolvedVersion-$TargetTriple.zip"
}

function New-TemporaryDirectory {
    $path = Join-Path ([System.IO.Path]::GetTempPath()) ([System.Guid]::NewGuid().ToString('N'))
    [System.IO.Directory]::CreateDirectory($path) | Out-Null
    $path
}

function Expand-ReleaseArchive {
    param(
        [Parameter(Mandatory)] [string]$ArchivePath,
        [Parameter(Mandatory)] [string]$InstallDir
    )

    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
    Expand-Archive -Path $ArchivePath -DestinationPath $InstallDir -Force
}

function Get-InstalledBinaries {
    param([Parameter(Mandatory)] [string]$InstallDir)

    Get-ChildItem -LiteralPath  -Filter 'luchta*.exe' -File |
        Sort-Object -Property Name |
        ForEach-Object { $_.Name }
}

function Assert-CoreBinaryPresent {
    param([Parameter(Mandatory)] [string]$InstallDir)

    $coreBinary = Join-Path $InstallDir 'luchta.exe'
    if (-not (Test-Path -LiteralPath $coreBinary -PathType Leaf)) {
        Fail "Archive extraction incomplete. Core binary 'luchta.exe' is missing."
    }
}

function Test-PathContainsDir {
    param([Parameter(Mandatory)] [string]$InstallDir)

    $entries = @($env:PATH -split ';' | Where-Object { $_ -ne '' })
    foreach ($entry in $entries) {
        if ([string]::Equals($entry.TrimEnd('\'), $InstallDir.TrimEnd('\'), [System.StringComparison]::OrdinalIgnoreCase)) {
            return $true
        }
    }
    return $false
}

function Write-SuccessSummary {
    param(
        [Parameter(Mandatory)] [string]$InstallDir,
        [Parameter(Mandatory)] [string]$ResolvedVersion,
        [Parameter(Mandatory)] [string]$TargetTriple,
        [Parameter(Mandatory)] [string[]]$InstalledBinaries
    )

    Write-Host "Installed luchta $ResolvedVersion for $TargetTriple into $InstallDir"
    Write-Host 'Installed binaries:'
    foreach ($binary in $InstalledBinaries) {
        Write-Host "  - $binary"
    }

    if (Test-PathContainsDir -InstallDir $InstallDir) {
        Write-Host "PATH already contains $InstallDir"
    } else {
        Write-Host 'Add this directory to your user PATH to use luchta and bundled workers automatically:'
        Write-Host "  `$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')"
        Write-Host "  [Environment]::SetEnvironmentVariable('Path', '$InstallDir;' + `$userPath, 'User')"
        Write-Host 'Or add it manually through System Settings -> Environment Variables.'
    }

    Write-Host "luchta discovers bundled workers via PATH. Keeping all binaries in $InstallDir lets tsc/oxc/etc. tasks resolve automatically."
}

$TempDir = $null
try {
    $resolvedVersion = Get-ReleaseVersion -RequestedVersion $Version
    $targetTriple = Get-TargetTriple
    $archiveUrl = Get-ArchiveUrl -ResolvedVersion $resolvedVersion -TargetTriple $targetTriple

    $TempDir = New-TemporaryDirectory
    $archivePath = Join-Path $TempDir 'luchta.zip'

    Write-Host "Downloading $archiveUrl"
    try {
        Invoke-WebRequest -Uri $archiveUrl -Headers (Get-AuthHeaders) -OutFile $archivePath
    } catch {
        Fail "Failed to download release archive. $($_.Exception.Message)"
    }

    if (-not (Test-Path -LiteralPath $archivePath -PathType Leaf) -or ((Get-Item -LiteralPath $archivePath).Length -le 0)) {
        Fail 'Downloaded archive is empty.'
    }

    Expand-ReleaseArchive -ArchivePath $archivePath -InstallDir $Dir
    Assert-CoreBinaryPresent -InstallDir $Dir
    $installedBinaries = @(Get-InstalledBinaries -InstallDir $Dir)
    Write-SuccessSummary -InstallDir $Dir -ResolvedVersion $resolvedVersion -TargetTriple $targetTriple -InstalledBinaries $installedBinaries
} finally {
    if ($TempDir -and (Test-Path -LiteralPath $TempDir)) {
        Remove-Item -LiteralPath $TempDir -Recurse -Force -ErrorAction SilentlyContinue
    }
}
