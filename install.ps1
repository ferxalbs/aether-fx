[CmdletBinding()]
param(
    [Parameter(Mandatory = $false)]
    [string] $Version,

    [Parameter(Mandatory = $false)]
    [string] $Dir
)

$ErrorActionPreference = 'Stop'

function Fail([string] $Message) {
    throw "AETHER installer: $Message"
}

if ([string]::IsNullOrWhiteSpace($Version)) {
    Fail 'an explicit release version is required; use -Version v0.1.0-alpha-07'
}
if ($Version -notmatch '^v[0-9]+\.[0-9]+\.[0-9]+(?:[-+][0-9A-Za-z.-]+)?$') {
    Fail "invalid release version: $Version"
}
if ($env:OS -ne 'Windows_NT') {
    Fail 'install.ps1 supports Windows only'
}

$architecture = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
switch ($architecture) {
    'X64' { $platform = 'windows-x86_64' }
    'Arm64' { $platform = 'windows-aarch64' }
    default { Fail "unsupported Windows architecture: $architecture; supported architectures are x64 and ARM64" }
}

$archive = "aether-$Version-$platform.zip"
$releaseUrl = "https://github.com/ferxalbs/aether-fx/releases/download/$Version"
if ([string]::IsNullOrWhiteSpace($Dir)) {
    $installDirectory = Join-Path $env:LOCALAPPDATA 'AETHER\bin'
} else {
    $installDirectory = [System.IO.Path]::GetFullPath($Dir)
}

$temporaryDirectory = Join-Path ([System.IO.Path]::GetTempPath()) ("aether-install-" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $temporaryDirectory | Out-Null

try {
    $archivePath = Join-Path $temporaryDirectory $archive
    $checksumsPath = Join-Path $temporaryDirectory 'SHA256SUMS'
    Invoke-WebRequest -UseBasicParsing -Uri "$releaseUrl/$archive" -OutFile $archivePath
    Invoke-WebRequest -UseBasicParsing -Uri "$releaseUrl/SHA256SUMS" -OutFile $checksumsPath

    $checksumLines = @(Get-Content -LiteralPath $checksumsPath | Where-Object {
        $parts = $_ -split '\s+', 2
        $parts.Count -eq 2 -and $parts[1] -eq $archive
    })
    if ($checksumLines.Count -ne 1) {
        Fail "no unique checksum found for $archive"
    }
    $expectedHash = (($checksumLines[0] -split '\s+', 2)[0]).ToLowerInvariant()
    $actualHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $archivePath).Hash.ToLowerInvariant()
    if ($actualHash -ne $expectedHash) {
        Fail "checksum verification failed for $archive"
    }

    $extractedDirectory = Join-Path $temporaryDirectory 'extracted'
    Expand-Archive -LiteralPath $archivePath -DestinationPath $extractedDirectory
    $binaryPath = Join-Path $extractedDirectory 'aether.exe'
    if (-not (Test-Path -LiteralPath $binaryPath -PathType Leaf)) {
        Fail 'release archive does not contain aether.exe'
    }

    New-Item -ItemType Directory -Force -Path $installDirectory | Out-Null
    $installedPath = Join-Path $installDirectory 'aether.exe'
    Copy-Item -LiteralPath $binaryPath -Destination $installedPath -Force

    $installedVersion = (& $installedPath --version 2>&1 | Out-String).Trim()
    if ($LASTEXITCODE -ne 0 -or $installedVersion -ne $Version.Substring(1)) {
        Fail "installed binary reported $installedVersion, expected $($Version.Substring(1))"
    }

    Write-Output "Installed AETHER Fx $($Version.Substring(1)) at $installedPath"
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    $pathEntries = @($userPath -split ';' | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
    if ($pathEntries -notcontains $installDirectory) {
        Write-Output "Add this directory to your user PATH: $installDirectory"
        Write-Output 'For the current PowerShell session: $env:Path = "' + $installDirectory + ';$env:Path"'
    }
} finally {
    if (Test-Path -LiteralPath $temporaryDirectory) {
        Remove-Item -LiteralPath $temporaryDirectory -Recurse -Force
    }
}
