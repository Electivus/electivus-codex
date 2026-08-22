[CmdletBinding()]
param(
    [switch]$UseUpstreamVersion,
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$RemainingArguments
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

if ($PSVersionTable.PSVersion.Major -lt 7) {
    throw "install-local.ps1 requires PowerShell 7 or newer. Run it with pwsh."
}

function Write-Step {
    param(
        [string]$Message
    )

    Write-Host "==> $Message"
}

function Show-Usage {
    Write-Host @"
Usage: install-local.ps1 [-UseUpstreamVersion]

  -UseUpstreamVersion  Build using the greatest upstream release or pre-release
                        version in the current commit's ancestry instead of 0.0.0.

The CODEX_HOME environment variable selects the standalone installation root.
CODEX_INSTALL_DIR selects the directory exposed on PATH.
PowerShell 7 or newer is required; run this script with pwsh.
"@
}

$useUpstreamVersionRequested = $UseUpstreamVersion.IsPresent
if ($null -ne $RemainingArguments) {
    foreach ($argument in $RemainingArguments) {
        switch ($argument) {
            "--help" {
                Show-Usage
                exit 0
            }
            "-h" {
                Show-Usage
                exit 0
            }
            "--use-upstream-version" {
                $useUpstreamVersionRequested = $true
            }
            default {
                throw "Unknown argument: $argument"
            }
        }
    }
}

function Resolve-CommandPath {
    param(
        [string[]]$Names,
        [string]$Description
    )

    foreach ($name in $Names) {
        $command = Get-Command -Name $name -CommandType Application -ErrorAction SilentlyContinue |
            Select-Object -First 1
        if ($null -ne $command) {
            return $command.Path
        }
    }

    throw "$Description is required to install a local Codex release build."
}

function ConvertTo-AbsolutePath {
    param(
        [string]$Path
    )

    return $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($Path)
}

function Path-Contains {
    param(
        [string]$PathValue,
        [string]$Entry
    )

    if ([string]::IsNullOrWhiteSpace($PathValue)) {
        return $false
    }

    $needle = $Entry.TrimEnd("\")
    foreach ($segment in $PathValue.Split(";", [System.StringSplitOptions]::RemoveEmptyEntries)) {
        if ($segment.TrimEnd("\") -ieq $needle) {
            return $true
        }
    }

    return $false
}

function Prepend-PathEntry {
    param(
        [string]$PathValue,
        [string]$Entry
    )

    $needle = $Entry.TrimEnd("\")
    $segments = @($Entry)
    if (-not [string]::IsNullOrWhiteSpace($PathValue)) {
        $segments += @(
            $PathValue.Split(";", [System.StringSplitOptions]::RemoveEmptyEntries) |
                Where-Object { $_.TrimEnd("\") -ine $needle }
        )
    }

    return ($segments -join ";")
}

function Invoke-WithInstallLock {
    param(
        [string]$LockPath,
        [scriptblock]$Script
    )

    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $LockPath) | Out-Null
    $lock = $null
    while ($null -eq $lock) {
        try {
            $lock = [System.IO.File]::Open(
                $LockPath,
                [System.IO.FileMode]::OpenOrCreate,
                [System.IO.FileAccess]::ReadWrite,
                [System.IO.FileShare]::None
            )
        } catch [System.IO.IOException] {
            Start-Sleep -Milliseconds 250
        }
    }

    try {
        & $Script
    } finally {
        $lock.Dispose()
    }
}

function Remove-StaleInstallArtifacts {
    param(
        [string]$StandaloneRoot,
        [string]$ReleasesDir
    )

    if (Test-Path -LiteralPath $ReleasesDir -PathType Container) {
        $staleStaging = @(
            Get-ChildItem -LiteralPath $ReleasesDir -Force -Directory |
                Where-Object { $_.Name -like ".staging.*" }
        )
        foreach ($item in $staleStaging) {
            Remove-Item -LiteralPath $item.FullName -Recurse -Force
        }
    }

    if (Test-Path -LiteralPath $StandaloneRoot -PathType Container) {
        $staleLinks = @(
            Get-ChildItem -LiteralPath $StandaloneRoot -Force |
                Where-Object {
                    $_.Name -like ".current.*" -or $_.Name -like ".swap-backup.*"
                }
        )
        foreach ($item in $staleLinks) {
            Remove-Item -LiteralPath $item.FullName -Recurse -Force
        }
    }
}

function Add-JunctionSupportType {
    if (([System.Management.Automation.PSTypeName]'CodexInstaller.Junction').Type) {
        return
    }

    Add-Type -TypeDefinition @"
using System;
using System.ComponentModel;
using System.IO;
using System.Runtime.InteropServices;
using System.Text;
using Microsoft.Win32.SafeHandles;

namespace CodexInstaller
{
    public static class Junction
    {
        private const uint GENERIC_WRITE = 0x40000000;
        private const uint FILE_SHARE_READ = 0x00000001;
        private const uint FILE_SHARE_WRITE = 0x00000002;
        private const uint FILE_SHARE_DELETE = 0x00000004;
        private const uint OPEN_EXISTING = 3;
        private const uint FILE_FLAG_BACKUP_SEMANTICS = 0x02000000;
        private const uint FILE_FLAG_OPEN_REPARSE_POINT = 0x00200000;
        private const uint FSCTL_SET_REPARSE_POINT = 0x000900A4;
        private const uint IO_REPARSE_TAG_MOUNT_POINT = 0xA0000003;
        private const int HeaderLength = 20;

        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        private static extern SafeFileHandle CreateFileW(
            string lpFileName,
            uint dwDesiredAccess,
            uint dwShareMode,
            IntPtr lpSecurityAttributes,
            uint dwCreationDisposition,
            uint dwFlagsAndAttributes,
            IntPtr hTemplateFile);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool DeviceIoControl(
            SafeFileHandle hDevice,
            uint dwIoControlCode,
            byte[] lpInBuffer,
            int nInBufferSize,
            IntPtr lpOutBuffer,
            int nOutBufferSize,
            out int lpBytesReturned,
            IntPtr lpOverlapped);

        public static void SetTarget(string linkPath, string targetPath)
        {
            string substituteName = "\\??\\" + Path.GetFullPath(targetPath);
            byte[] substituteNameBytes = Encoding.Unicode.GetBytes(substituteName);
            if (substituteNameBytes.Length > ushort.MaxValue - HeaderLength) {
                throw new ArgumentException("Junction target path is too long.", "targetPath");
            }

            byte[] reparseBuffer = new byte[substituteNameBytes.Length + HeaderLength];
            WriteUInt32(reparseBuffer, 0, IO_REPARSE_TAG_MOUNT_POINT);
            WriteUInt16(reparseBuffer, 4, checked((ushort)(substituteNameBytes.Length + 12)));
            WriteUInt16(reparseBuffer, 8, 0);
            WriteUInt16(reparseBuffer, 10, checked((ushort)substituteNameBytes.Length));
            WriteUInt16(reparseBuffer, 12, checked((ushort)(substituteNameBytes.Length + 2)));
            WriteUInt16(reparseBuffer, 14, 0);
            Buffer.BlockCopy(substituteNameBytes, 0, reparseBuffer, 16, substituteNameBytes.Length);

            using (SafeFileHandle handle = CreateFileW(
                linkPath,
                GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                IntPtr.Zero,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                IntPtr.Zero))
            {
                if (handle.IsInvalid) {
                    throw new Win32Exception(Marshal.GetLastWin32Error());
                }

                int bytesReturned;
                if (!DeviceIoControl(
                    handle,
                    FSCTL_SET_REPARSE_POINT,
                    reparseBuffer,
                    reparseBuffer.Length,
                    IntPtr.Zero,
                    0,
                    out bytesReturned,
                    IntPtr.Zero))
                {
                    throw new Win32Exception(Marshal.GetLastWin32Error());
                }
            }
        }

        private static void WriteUInt16(byte[] buffer, int offset, ushort value)
        {
            buffer[offset] = (byte)value;
            buffer[offset + 1] = (byte)(value >> 8);
        }

        private static void WriteUInt32(byte[] buffer, int offset, uint value)
        {
            buffer[offset] = (byte)value;
            buffer[offset + 1] = (byte)(value >> 8);
            buffer[offset + 2] = (byte)(value >> 16);
            buffer[offset + 3] = (byte)(value >> 24);
        }
    }
}
"@
}

function Set-JunctionTarget {
    param(
        [string]$LinkPath,
        [string]$TargetPath
    )

    Add-JunctionSupportType
    [CodexInstaller.Junction]::SetTarget($LinkPath, $TargetPath)
}

function Test-IsJunction {
    param(
        [string]$Path
    )

    $item = Get-Item -LiteralPath $Path -Force
    return ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -and
        $item.LinkType -eq "Junction"
}

function Test-TargetOwnedByInstaller {
    param(
        [string]$TargetPath,
        [string]$OwnedTargetPrefix
    )

    if ([string]::IsNullOrWhiteSpace($OwnedTargetPrefix)) {
        return $true
    }

    $target = ConvertTo-AbsolutePath $TargetPath
    $prefix = (ConvertTo-AbsolutePath $OwnedTargetPrefix).TrimEnd("\")
    return $target.Equals($prefix, [System.StringComparison]::OrdinalIgnoreCase) -or
        $target.StartsWith($prefix + "\", [System.StringComparison]::OrdinalIgnoreCase)
}

function Ensure-Junction {
    param(
        [string]$LinkPath,
        [string]$TargetPath,
        [string]$InstallerOwnedTargetPrefix
    )

    $resolvedTargetPath = ConvertTo-AbsolutePath $TargetPath
    $item = Get-Item -LiteralPath $LinkPath -Force -ErrorAction SilentlyContinue
    if ($null -eq $item) {
        New-Item -ItemType Junction -Path $LinkPath -Target $resolvedTargetPath | Out-Null
        return
    }

    if (Test-IsJunction -Path $LinkPath) {
        $existingTarget = [string]$item.Target
        if (
            -not (
                Test-TargetOwnedByInstaller `
                    -TargetPath $existingTarget `
                    -OwnedTargetPrefix $InstallerOwnedTargetPrefix
            )
        ) {
            throw "Refusing to retarget junction at $LinkPath because it is not managed by this installer."
        }
        if ($existingTarget.Equals($resolvedTargetPath, [System.StringComparison]::OrdinalIgnoreCase)) {
            return
        }

        Set-JunctionTarget -LinkPath $LinkPath -TargetPath $resolvedTargetPath
        return
    }

    if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) {
        throw "Refusing to replace non-junction reparse point at $LinkPath."
    }

    if ($item.PSIsContainer) {
        if ($null -ne (Get-ChildItem -LiteralPath $LinkPath -Force | Select-Object -First 1)) {
            throw "Refusing to replace non-empty directory at $LinkPath with a junction."
        }

        Remove-Item -LiteralPath $LinkPath -Force
        New-Item -ItemType Junction -Path $LinkPath -Target $resolvedTargetPath | Out-Null
        return
    }

    throw "Refusing to replace file at $LinkPath with a junction."
}

function Test-LocalPackageComplete {
    param(
        [string]$PackageDir,
        [string]$ExpectedTarget
    )

    if (-not (Test-Path -LiteralPath $PackageDir -PathType Container)) {
        return $false
    }

    $expectedFiles = @(
        "codex-package.json",
        "bin\codex.exe",
        "bin\codex-code-mode-host.exe",
        "codex-path\rg.exe",
        "codex-resources\codex-command-runner.exe",
        "codex-resources\codex-windows-sandbox-setup.exe"
    )
    foreach ($name in $expectedFiles) {
        if (-not (Test-Path -LiteralPath (Join-Path $PackageDir $name) -PathType Leaf)) {
            return $false
        }
    }

    try {
        $metadata = Get-Content -LiteralPath (Join-Path $PackageDir "codex-package.json") -Raw |
            ConvertFrom-Json
    } catch {
        return $false
    }

    $propertyNames = @($metadata.PSObject.Properties.Name)
    foreach ($propertyName in @("layoutVersion", "target", "variant", "entrypoint")) {
        if ($propertyNames -notcontains $propertyName) {
            return $false
        }
    }

    return $metadata.layoutVersion -eq 1 -and
        $metadata.target -ceq $ExpectedTarget -and
        $metadata.variant -ceq "codex" -and
        $metadata.entrypoint -ceq "bin/codex.exe"
}

function Test-VisibleCodexCommand {
    param(
        [string]$VisibleBinDir
    )

    $codexCommand = Join-Path $VisibleBinDir "codex.exe"
    if (-not (Test-Path -LiteralPath $codexCommand -PathType Leaf)) {
        throw "Installed Codex command was not found: $codexCommand"
    }

    & $codexCommand --version *> $null
    if ($LASTEXITCODE -ne 0) {
        throw "Installed Codex command failed verification: $codexCommand --version"
    }
}

function Invoke-LocalPackageBuild {
    param(
        [string]$PackageDir,
        [string]$Target,
        [string]$BuildScript,
        [string]$CargoPath,
        [string]$PythonPath,
        [string[]]$PythonPrefixArguments
    )

    Write-Step "Building local Codex release package"
    if (Test-Path -LiteralPath $PackageDir) {
        Remove-Item -LiteralPath $PackageDir -Recurse -Force
    }

    $buildArguments = @(
        $BuildScript,
        "--cargo",
        $CargoPath,
        "--target",
        $Target,
        "--variant",
        "codex",
        "--cargo-profile",
        "release",
        "--package-dir",
        $PackageDir,
        "--force"
    )
    if (-not [string]::IsNullOrWhiteSpace($env:CODEX_LOCAL_RG)) {
        $localRgPath = ConvertTo-AbsolutePath $env:CODEX_LOCAL_RG
        if (-not (Test-Path -LiteralPath $localRgPath -PathType Leaf)) {
            throw "CODEX_LOCAL_RG must point to an executable rg."
        }
        $buildArguments += @("--rg-bin", $localRgPath)
    }

    $commandArguments = @($PythonPrefixArguments) + $buildArguments
    & $PythonPath @commandArguments
    if ($LASTEXITCODE -ne 0) {
        throw "Local Codex package build failed with exit code $LASTEXITCODE."
    }
}

function Read-WorkspaceVersion {
    param(
        [string]$CargoManifestPath
    )

    $text = [System.IO.File]::ReadAllText($CargoManifestPath)
    $pattern = '(?ms)(^\[workspace\.package\]\s+(?:(?!^\[).)*?^\s*version\s*=\s*")([^"]+)(")'
    $match = [regex]::Match($text, $pattern)
    if (-not $match.Success) {
        throw "Could not find [workspace.package].version in $CargoManifestPath."
    }

    return $match.Groups[2].Value
}

function Set-WorkspaceVersion {
    param(
        [string]$CargoManifestPath,
        [string]$Version
    )

    $text = [System.IO.File]::ReadAllText($CargoManifestPath)
    $pattern = '(?ms)(^\[workspace\.package\]\s+(?:(?!^\[).)*?^\s*version\s*=\s*")([^"]+)(")'
    $match = [regex]::Match($text, $pattern)
    if (-not $match.Success) {
        throw "Could not find [workspace.package].version in $CargoManifestPath."
    }

    $versionStart = $match.Groups[2].Index
    $updatedText = $text.Remove($versionStart, $match.Groups[2].Length).Insert($versionStart, $Version)
    [System.IO.File]::WriteAllText(
        $CargoManifestPath,
        $updatedText,
        [System.Text.UTF8Encoding]::new($false)
    )
}

function ConvertTo-SemanticVersion {
    param(
        [string]$Version
    )

    $pattern = '^(?<major>0|[1-9]\d*)\.(?<minor>0|[1-9]\d*)\.(?<patch>0|[1-9]\d*)(?:-(?<prerelease>(?:0|[1-9]\d*|\d*[A-Za-z-][0-9A-Za-z-]*)(?:\.(?:0|[1-9]\d*|\d*[A-Za-z-][0-9A-Za-z-]*))*))?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$'
    $match = [regex]::Match($Version, $pattern)
    if (-not $match.Success) {
        return $null
    }

    try {
        return [System.Management.Automation.SemanticVersion]::new($Version)
    } catch {
        return $null
    }
}

function Get-UpstreamBuildVersion {
    param(
        [string]$GitPath,
        [string]$RepoRoot
    )

    $entries = & $GitPath -C $RepoRoot log --full-history --format=%H%x09%s HEAD -- "codex-rs/Cargo.toml"
    if ($LASTEXITCODE -ne 0) {
        throw "Could not inspect repository history for the upstream release version."
    }

    $releasePattern = '^Release (?<version>.+)$'
    $workspaceVersionPattern = '(?ms)(^\[workspace\.package\]\s+(?:(?!^\[).)*?^\s*version\s*=\s*")([^"]+)(")'
    $selectedVersion = $null
    $selectedVersionText = $null
    foreach ($entry in @($entries)) {
        $parts = ([string]$entry).Split("`t", 2)
        if ($parts.Count -ne 2) {
            continue
        }

        $releaseMatch = [regex]::Match($parts[1], $releasePattern)
        if (-not $releaseMatch.Success) {
            continue
        }

        $versionText = $releaseMatch.Groups["version"].Value
        $semanticVersion = ConvertTo-SemanticVersion -Version $versionText
        if ($null -eq $semanticVersion -or $versionText -eq "0.0.0") {
            continue
        }

        $manifestLines = & $GitPath -C $RepoRoot show "$($parts[0]):codex-rs/Cargo.toml"
        if ($LASTEXITCODE -ne 0) {
            throw "Could not inspect Cargo.toml at release candidate $($parts[0])."
        }
        $manifestMatch = [regex]::Match(
            ($manifestLines -join "`n"),
            $workspaceVersionPattern
        )
        if (
            -not $manifestMatch.Success -or
            $manifestMatch.Groups[2].Value -cne $versionText
        ) {
            continue
        }

        if (
            $null -eq $selectedVersion -or
            $semanticVersion.CompareTo($selectedVersion) -gt 0
        ) {
            $selectedVersion = $semanticVersion
            $selectedVersionText = $versionText
        }
    }

    if ($null -eq $selectedVersion) {
        throw "Could not find a valid upstream release version in HEAD's ancestry."
    }
    return $selectedVersionText
}

function Backup-CargoManifestFiles {
    param(
        [string]$CargoManifestPath,
        [string]$CargoLockPath,
        [string]$BackupDir
    )

    $manifestBackupPath = Join-Path $BackupDir "Cargo.toml.original"
    Copy-Item -LiteralPath $CargoManifestPath -Destination $manifestBackupPath -Force

    $lockExisted = Test-Path -LiteralPath $CargoLockPath -PathType Leaf
    $lockBackupPath = $null
    if ($lockExisted) {
        $lockBackupPath = Join-Path $BackupDir "Cargo.lock.original"
        Copy-Item -LiteralPath $CargoLockPath -Destination $lockBackupPath -Force
    }

    return [PSCustomObject]@{
        ManifestBackupPath = $manifestBackupPath
        LockBackupPath = $lockBackupPath
        LockExisted = $lockExisted
    }
}

function Restore-CargoManifestFiles {
    param(
        [object]$Backups,
        [string]$CargoManifestPath,
        [string]$CargoLockPath
    )

    Copy-Item -LiteralPath $Backups.ManifestBackupPath -Destination $CargoManifestPath -Force
    if ($Backups.LockExisted) {
        Copy-Item -LiteralPath $Backups.LockBackupPath -Destination $CargoLockPath -Force
    } elseif (Test-Path -LiteralPath $CargoLockPath) {
        Remove-Item -LiteralPath $CargoLockPath -Force
    }
}

function Prune-OldReleases {
    param(
        [string]$ReleasesDir,
        [string]$ActiveRelease
    )

    $resolvedReleasesDir = (ConvertTo-AbsolutePath $ReleasesDir).TrimEnd("\")
    $resolvedActiveRelease = ConvertTo-AbsolutePath $ActiveRelease
    if (
        [System.IO.Path]::GetDirectoryName($resolvedActiveRelease).TrimEnd("\") -ine $resolvedReleasesDir -or
        -not (Test-Path -LiteralPath $resolvedActiveRelease -PathType Container)
    ) {
        throw "Refusing to prune around invalid active release: $resolvedActiveRelease"
    }

    $candidates = @(
        Get-ChildItem -LiteralPath $resolvedReleasesDir -Force -Directory |
            Where-Object {
                $_.FullName -ine $resolvedActiveRelease -and
                    $_.Name -notlike ".*" -and
                    -not ($_.Attributes -band [IO.FileAttributes]::ReparsePoint)
            } |
            ForEach-Object {
                $timestamp = $null
                $priority = 0
                if ($_.Name -match "-(?<timestamp>[0-9]{14})-[0-9]+$") {
                    $timestamp = $matches["timestamp"]
                    $priority = 1
                }

                [PSCustomObject]@{
                    Path = $_
                    Priority = $priority
                    Timestamp = $timestamp
                    Modified = $_.LastWriteTimeUtc
                }
            }
    )

    $sortedCandidates = @(
        $candidates | Sort-Object `
            @{ Expression = { $_.Priority }; Descending = $true }, `
            @{ Expression = {
                    if ($_.Priority -eq 1) {
                        $_.Timestamp
                    } else {
                        $_.Modified
                    }
                }; Descending = $true }, `
            @{ Expression = { $_.Path.Name }; Descending = $true }
    )

    foreach ($candidate in @($sortedCandidates | Select-Object -Skip 2)) {
        Remove-Item -LiteralPath $candidate.Path.FullName -Recurse -Force
        Write-Host "Removed old standalone release: $($candidate.Path.Name)"
    }
}

if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) {
    throw "install-local.ps1 supports Windows only. Use install-local.sh on macOS or Linux."
}

if (-not [Environment]::Is64BitOperatingSystem) {
    throw "Codex requires a 64-bit version of Windows."
}

$architecture = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture
switch ($architecture) {
    "Arm64" {
        $target = "aarch64-pc-windows-msvc"
    }
    "X64" {
        $target = "x86_64-pc-windows-msvc"
    }
    default {
        throw "Unsupported architecture: $architecture"
    }
}

$scriptDir = ConvertTo-AbsolutePath $PSScriptRoot
$repoRoot = ConvertTo-AbsolutePath (Join-Path $scriptDir "..\..")
$env:CODEX_REPO_ROOT = $repoRoot
$buildScript = Join-Path $repoRoot "scripts\build_codex_package.py"
$cargoManifestPath = Join-Path $repoRoot "codex-rs\Cargo.toml"
$cargoLockPath = Join-Path $repoRoot "codex-rs\Cargo.lock"

if (-not (Test-Path -LiteralPath $buildScript -PathType Leaf)) {
    throw "Could not find the local package builder: $buildScript"
}
if (-not (Test-Path -LiteralPath $cargoManifestPath -PathType Leaf)) {
    throw "Could not find the Cargo workspace manifest: $cargoManifestPath"
}

$cargoPath = Resolve-CommandPath -Names @("cargo") -Description "cargo"
$pythonPath = $null
$pythonPrefixArguments = @()
$pythonCommand = Get-Command -Name "python" -CommandType Application -ErrorAction SilentlyContinue |
    Select-Object -First 1
if ($null -ne $pythonCommand) {
    $pythonPath = $pythonCommand.Path
} else {
    $pythonPath = Resolve-CommandPath -Names @("py") -Description "python or py"
    $pythonPrefixArguments = @("-3")
}

$codexHome = if ([string]::IsNullOrWhiteSpace($env:CODEX_HOME)) {
    Join-Path $env:USERPROFILE ".codex"
} else {
    $env:CODEX_HOME
}
$codexHome = ConvertTo-AbsolutePath $codexHome
$standaloneRoot = Join-Path $codexHome "packages\standalone"
$releasesDir = Join-Path $standaloneRoot "releases"
$currentDir = Join-Path $standaloneRoot "current"
$lockPath = Join-Path $standaloneRoot "install.lock"

$defaultVisibleBinDir = Join-Path $env:LOCALAPPDATA "Programs\OpenAI\Codex\bin"
$visibleBinDir = if ([string]::IsNullOrWhiteSpace($env:CODEX_INSTALL_DIR)) {
    $defaultVisibleBinDir
} else {
    $env:CODEX_INSTALL_DIR
}
$visibleBinDir = ConvertTo-AbsolutePath $visibleBinDir

$releasePrefix = "local-release-$target"
$releaseName = "$releasePrefix-$([DateTime]::UtcNow.ToString('yyyyMMddHHmmss'))-$PID"
$releaseDir = Join-Path $releasesDir $releaseName
$stagingDir = Join-Path $releasesDir ".staging.$releaseName.$PID"
$tempDir = Join-Path ([System.IO.Path]::GetTempPath()) ("codex-local-install-" + [Guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Force -Path $tempDir | Out-Null

try {
    Invoke-WithInstallLock -LockPath $lockPath -Script {
        New-Item -ItemType Directory -Force -Path $standaloneRoot | Out-Null
        New-Item -ItemType Directory -Force -Path $releasesDir | Out-Null
        Remove-StaleInstallArtifacts -StandaloneRoot $standaloneRoot -ReleasesDir $releasesDir
        Write-Step "Installing local release build to $releaseDir"

        if ($useUpstreamVersionRequested) {
            $currentWorkspaceVersion = Read-WorkspaceVersion -CargoManifestPath $cargoManifestPath
            if ($currentWorkspaceVersion -eq "0.0.0") {
                $gitPath = Resolve-CommandPath -Names @("git") -Description "git"
                $upstreamBuildVersion = Get-UpstreamBuildVersion -GitPath $gitPath -RepoRoot $repoRoot
                Write-Step "Using upstream release version $upstreamBuildVersion for local build"
                $cargoBackups = Backup-CargoManifestFiles `
                    -CargoManifestPath $cargoManifestPath `
                    -CargoLockPath $cargoLockPath `
                    -BackupDir $tempDir
                try {
                    Set-WorkspaceVersion -CargoManifestPath $cargoManifestPath -Version $upstreamBuildVersion
                    Invoke-LocalPackageBuild `
                        -PackageDir $stagingDir `
                        -Target $target `
                        -BuildScript $buildScript `
                        -CargoPath $cargoPath `
                        -PythonPath $pythonPath `
                        -PythonPrefixArguments $pythonPrefixArguments
                } finally {
                    Restore-CargoManifestFiles `
                        -Backups $cargoBackups `
                        -CargoManifestPath $cargoManifestPath `
                        -CargoLockPath $cargoLockPath
                }
            } else {
                Write-Step "Using existing workspace version $currentWorkspaceVersion for local build"
                Invoke-LocalPackageBuild `
                    -PackageDir $stagingDir `
                    -Target $target `
                    -BuildScript $buildScript `
                    -CargoPath $cargoPath `
                    -PythonPath $pythonPath `
                    -PythonPrefixArguments $pythonPrefixArguments
            }
        } else {
            Invoke-LocalPackageBuild `
                -PackageDir $stagingDir `
                -Target $target `
                -BuildScript $buildScript `
                -CargoPath $cargoPath `
                -PythonPath $pythonPath `
                -PythonPrefixArguments $pythonPrefixArguments
        }

        if (-not (Test-LocalPackageComplete -PackageDir $stagingDir -ExpectedTarget $target)) {
            Remove-Item -LiteralPath $stagingDir -Recurse -Force -ErrorAction SilentlyContinue
            throw "Local release validation failed."
        }

        if (Test-Path -LiteralPath $releaseDir) {
            $existingRelease = Get-Item -LiteralPath $releaseDir -Force
            if ($existingRelease.Attributes -band [IO.FileAttributes]::ReparsePoint) {
                throw "Refusing to replace reparse point at $releaseDir."
            }
            Remove-Item -LiteralPath $releaseDir -Recurse -Force
        }
        Move-Item -LiteralPath $stagingDir -Destination $releaseDir

        Ensure-Junction `
            -LinkPath $currentDir `
            -TargetPath $releaseDir `
            -InstallerOwnedTargetPrefix $releasesDir

        $visibleParent = Split-Path -Parent $visibleBinDir
        New-Item -ItemType Directory -Force -Path $visibleParent | Out-Null
        Ensure-Junction `
            -LinkPath $visibleBinDir `
            -TargetPath (Join-Path $currentDir "bin") `
            -InstallerOwnedTargetPrefix $standaloneRoot
        Test-VisibleCodexCommand -VisibleBinDir $visibleBinDir
        Prune-OldReleases -ReleasesDir $releasesDir -ActiveRelease $releaseDir
    }
} finally {
    if (Test-Path -LiteralPath $tempDir) {
        Remove-Item -LiteralPath $tempDir -Recurse -Force -ErrorAction SilentlyContinue
    }
}

$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
if (-not (Path-Contains -PathValue $userPath -Entry $visibleBinDir)) {
    $newUserPath = Prepend-PathEntry -PathValue $userPath -Entry $visibleBinDir
    [Environment]::SetEnvironmentVariable("Path", $newUserPath, "User")
    Write-Step "PATH updated for future PowerShell sessions."
} elseif (Path-Contains -PathValue $env:Path -Entry $visibleBinDir) {
    Write-Step "$visibleBinDir is already on PATH."
} else {
    Write-Step "PATH is already configured for future PowerShell sessions."
}

if (-not (Path-Contains -PathValue $env:Path -Entry $visibleBinDir)) {
    $env:Path = Prepend-PathEntry -PathValue $env:Path -Entry $visibleBinDir
}

Write-Step "Current PowerShell session: codex"
Write-Step "Future PowerShell windows: open a new PowerShell window and run: codex"
Write-Host "Local Codex release build installed successfully."
