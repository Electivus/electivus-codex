param(
    [Parameter(Mandatory = $true)]
    [string]$Source,

    [Parameter(Mandatory = $true)]
    [string]$Destination
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$sourceRoot = [IO.Path]::GetFullPath($Source).TrimEnd('\')
$destinationRoot = [IO.Path]::GetFullPath($Destination).TrimEnd('\')
$comparison = [StringComparison]::OrdinalIgnoreCase

if (-not (Test-Path -LiteralPath $sourceRoot -PathType Container)) {
    throw "Windows CI source workspace does not exist: $sourceRoot"
}
if (Test-Path -LiteralPath $destinationRoot) {
    throw "Stable Windows CI workspace already exists: $destinationRoot"
}
if (
    $sourceRoot.Equals($destinationRoot, $comparison) -or
    $sourceRoot.StartsWith("$destinationRoot\", $comparison) -or
    $destinationRoot.StartsWith("$sourceRoot\", $comparison)
) {
    throw "Windows CI source and destination must not overlap"
}

New-Item -ItemType Directory -Path $destinationRoot | Out-Null
if (Test-Path Variable:PSNativeCommandUseErrorActionPreference) {
    $PSNativeCommandUseErrorActionPreference = $false
}
& robocopy.exe `
    $sourceRoot `
    $destinationRoot `
    /E `
    /COPY:DAT `
    /DCOPY:DAT `
    /R:2 `
    /W:1 `
    /NFL `
    /NDL `
    /NJH `
    /NJS `
    /NP
$robocopyExitCode = $LASTEXITCODE
if ($robocopyExitCode -gt 7) {
    throw "robocopy failed with exit code $robocopyExitCode"
}

foreach ($requiredPath in ('codex-rs', '.github\scripts')) {
    $copiedPath = Join-Path $destinationRoot $requiredPath
    if (-not (Test-Path -LiteralPath $copiedPath)) {
        throw "Stable Windows CI workspace is incomplete: $copiedPath"
    }
}

exit 0
