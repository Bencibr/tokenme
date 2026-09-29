[CmdletBinding()]
param(
    [switch]$ReleaseOnly,
    [string]$ToolsRoot = $(if ($env:TOKENME_TOOLS_ROOT) { $env:TOKENME_TOOLS_ROOT } else { 'E:\tokenme-tools' })
)

$ErrorActionPreference = 'Stop'
$repoRoot = Split-Path -Parent $PSScriptRoot
$cargoHome = Join-Path $ToolsRoot 'rust\cargo'
$rustupHome = Join-Path $ToolsRoot 'rust\rustup'
$cargoExe = Join-Path $cargoHome 'bin\cargo.exe'
$vsDevCmd = Join-Path $ToolsRoot 'vs-buildtools\Common7\Tools\VsDevCmd.bat'

if (-not (Test-Path -LiteralPath $cargoExe)) {
    throw "Rust is not installed at $cargoExe. Install the MSVC toolchain under TOKENME_TOOLS_ROOT first."
}
if (-not (Test-Path -LiteralPath $vsDevCmd)) {
    throw "Visual Studio Build Tools are not installed at $vsDevCmd."
}

$env:CARGO_HOME = $cargoHome
$env:RUSTUP_HOME = $rustupHome
$env:Path = "$(Join-Path $cargoHome 'bin');$env:Path"

# Import the x64 MSVC environment emitted by the official developer command
# file so cargo uses cl.exe/link.exe instead of looking for a Unix toolchain.
$dump = cmd.exe /d /s /c "call `"$vsDevCmd`" -arch=x64 >nul && set"
if ($LASTEXITCODE -ne 0) {
    throw "VsDevCmd.bat failed with exit code $LASTEXITCODE."
}
foreach ($line in $dump) {
    $separator = $line.IndexOf('=')
    if ($separator -gt 0) {
        Set-Item -Path ("Env:" + $line.Substring(0, $separator)) -Value $line.Substring($separator + 1)
    }
}
$env:Path = "$(Join-Path $cargoHome 'bin');$env:Path"

function Invoke-Cargo([string[]]$Arguments) {
    & $cargoExe @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "cargo $($Arguments -join ' ') failed with exit code $LASTEXITCODE."
    }
}

Push-Location $repoRoot
try {
    Invoke-Cargo @('build', '--release', '-p', 'usage-cli')
    if (-not $ReleaseOnly) {
        Invoke-Cargo @('test', '--workspace', '--all-targets')
    }
    & (Join-Path $repoRoot 'target\release\tokenme.exe') '--help'
    if ($LASTEXITCODE -ne 0) {
        throw "The Windows CLI smoke test failed with exit code $LASTEXITCODE."
    }
}
finally {
    Pop-Location
}
