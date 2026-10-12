[CmdletBinding()]
param(
    [ValidateSet('Enable', 'Disable', 'Status')][string]$Mode = 'Status',
    [ValidateSet('TokenMe-diagnostic.exe')][string]$ImageName = 'TokenMe-diagnostic.exe',
    [string]$GFlags = ''
)
$ErrorActionPreference = 'Stop'
if ($Mode -eq 'Status') {
    $machine = [Microsoft.Win32.RegistryKey]::OpenBaseKey([Microsoft.Win32.RegistryHive]::LocalMachine, [Microsoft.Win32.RegistryView]::Registry64)
    try {
        $key = $machine.OpenSubKey("SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\$ImageName")
        try {
            $raw = if ($key) { $key.GetValue('GlobalFlag', '0') } else { '0' }
            $flags = if ($raw -is [int]) { [uint32]$raw } else { [Convert]::ToUInt32(([string]$raw -replace '^0x', ''), 16) }
            @{ image=$ImageName; enabled=(($flags -band 0x02000000) -ne 0); restart_required_for_changes=$true } | ConvertTo-Json -Compress
        } finally { if ($key) { $key.Dispose() } }
    } finally { $machine.Dispose() }
    exit 0
}
if (-not $GFlags) {
    $architecture = if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') { 'arm64' } else { 'x64' }
    $GFlags = Join-Path ${env:ProgramFiles(x86)} "Windows Kits\10\Debuggers\$architecture\gflags.exe"
}
if (-not (Test-Path -LiteralPath $GFlags)) { throw 'Install Debugging Tools for Windows or pass -GFlags with its executable path.' }
switch ($Mode) {
    Enable { & $GFlags /p /enable $ImageName /full }
    Disable { & $GFlags /p /disable $ImageName }
}
if ($LASTEXITCODE -ne 0) { throw 'GFlags failed. Enable/Disable require administrator rights.' }
if ($Mode -ne 'Status') { Write-Output 'Restart the diagnostic copy for this change to take effect. Normal TokenMe.exe and the CLI are unaffected.' }
