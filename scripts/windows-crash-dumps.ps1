[CmdletBinding()]
param(
    [ValidateSet('Enable', 'Disable', 'Status')][string]$Mode = 'Status',
    [string]$InstallDirectory = ''
)
$ErrorActionPreference = 'Stop'
$dumpFolder = '%LOCALAPPDATA%\tokenme\crashes'
$ownerValue = 'TokenMeManagedCrashDumps'

function Set-TokenMeDumpPolicy([Microsoft.Win32.RegistryKey]$Parent, [string]$Image, [string]$Action) {
    $key = $Parent.OpenSubKey($Image, $Action -ne 'Status')
    try {
        if ($Action -eq 'Enable') {
            if ($key -and $key.GetValueNames().Count -and $key.GetValue($ownerValue, 0) -ne 1) {
                return @{ image = $Image; status = 'custom_policy_preserved' }
            }
            if ($key -and $key.GetValue($ownerValue, 0) -eq 1 -and (
                $key.GetValue('DumpFolder', '', [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames) -cne $dumpFolder -or
                $key.GetValue('DumpType', 0) -ne 2 -or $key.GetValue('DumpCount', 0) -ne 3 -or $key.GetValueNames().Count -ne 4)) {
                return @{ image = $Image; status = 'modified_policy_preserved' }
            }
            if (-not $key) { $key = $Parent.CreateSubKey($Image) }
            $key.SetValue('DumpFolder', $dumpFolder, [Microsoft.Win32.RegistryValueKind]::ExpandString)
            $key.SetValue('DumpType', 2, [Microsoft.Win32.RegistryValueKind]::DWord)
            $key.SetValue('DumpCount', 3, [Microsoft.Win32.RegistryValueKind]::DWord)
            $key.SetValue($ownerValue, 1, [Microsoft.Win32.RegistryValueKind]::DWord)
            return @{ image = $Image; status = 'full_dumps_enabled'; retained = 3 }
        }
        if ($Action -eq 'Disable') {
            if (-not $key -or $key.GetValue($ownerValue, 0) -ne 1) { return @{ image = $Image; status = 'not_owned' } }
            $rawFolder = $key.GetValue('DumpFolder', '', [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
            if ($rawFolder -cne $dumpFolder -or $key.GetValue('DumpType', 0) -ne 2 -or $key.GetValue('DumpCount', 0) -ne 3 -or $key.GetValueNames().Count -ne 4) {
                return @{ image = $Image; status = 'modified_policy_preserved' }
            }
            $key.Dispose(); $key = $null
            $Parent.DeleteSubKey($Image, $false)
            return @{ image = $Image; status = 'owned_policy_removed' }
        }
        return @{ image = $Image; status = $(if ($key -and $key.GetValue('DumpType', 0) -eq 2) { 'full_dumps_enabled' } else { 'not_configured' }) }
    } finally { if ($key) { $key.Dispose() } }
}

if ($MyInvocation.InvocationName -eq '.') { return }

try {
    if ($Mode -eq 'Enable') { [void][IO.Directory]::CreateDirectory([Environment]::ExpandEnvironmentVariables($dumpFolder)) }
    # A 32-bit NSIS PowerShell helper must still configure the 64-bit app's WER.
    $machine = [Microsoft.Win32.RegistryKey]::OpenBaseKey([Microsoft.Win32.RegistryHive]::LocalMachine, [Microsoft.Win32.RegistryView]::Registry64)
    try {
        $path = 'SOFTWARE\Microsoft\Windows\Windows Error Reporting\LocalDumps'
        $parent = if ($Mode -eq 'Enable') { $machine.CreateSubKey($path) } else { $machine.OpenSubKey($path, $Mode -eq 'Disable') }
        if (-not $parent) { @{ status = 'not_configured' } | ConvertTo-Json -Compress; exit 0 }
        try {
            if ($Mode -eq 'Disable' -and $InstallDirectory) {
                # The two installer modes can coexist on older machines. Leave
                # their shared image policy while the other installation exists.
                foreach ($hive in @($machine, [Microsoft.Win32.Registry]::CurrentUser)) {
                    $other = $hive.OpenSubKey('Software\Microsoft\Windows\CurrentVersion\Uninstall\TokenMe')
                    if ($other) {
                        try {
                            $location = ([string]$other.GetValue('InstallLocation', '')).Trim('"').TrimEnd('\')
                            if ($location -and $location -ine $InstallDirectory.TrimEnd('\') -and (Test-Path -LiteralPath (Join-Path $location 'TokenMe.exe'))) {
                                @{ status = 'other_installation_present' } | ConvertTo-Json -Compress; exit 0
                            }
                        } finally { $other.Dispose() }
                    }
                }
            }
            @('TokenMe.exe', 'TokenMe-diagnostic.exe') | ForEach-Object { Set-TokenMeDumpPolicy $parent $_ $Mode } | ConvertTo-Json -Compress
        } finally { $parent.Dispose() }
    } finally { $machine.Dispose() }
    exit 0
} catch {
    Write-Error ('Crash dump configuration failed; administrator rights are required for Enable/Disable. ' + $_.Exception.Message)
    exit 1
}
