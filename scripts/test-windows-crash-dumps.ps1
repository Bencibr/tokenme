$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'windows-crash-dumps.ps1') -Mode Status
$testPath = 'Software\TokenMe\Tests\CrashPolicy-' + [guid]::NewGuid().ToString('N')
$user = [Microsoft.Win32.RegistryKey]::OpenBaseKey([Microsoft.Win32.RegistryHive]::CurrentUser, [Microsoft.Win32.RegistryView]::Registry64)
$parent = $user.CreateSubKey($testPath)
try {
    $first = Set-TokenMeDumpPolicy $parent 'test.exe' Enable
    if ($first.status -ne 'full_dumps_enabled') { throw 'Enable failed' }
    $key = $parent.OpenSubKey('test.exe')
    try {
        if ($key.GetValue('DumpType') -ne 2 -or $key.GetValue('DumpCount') -ne 3) { throw 'Wrong dump settings' }
        if ($key.GetValueKind('DumpFolder') -ne [Microsoft.Win32.RegistryValueKind]::ExpandString) { throw 'Dump folder must expand for the crashing user' }
        if ($key.GetValue('DumpFolder', '', [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames) -cne '%LOCALAPPDATA%\tokenme\crashes') { throw 'Dump folder contains an installing user path' }
    } finally { $key.Dispose() }
    [void](Set-TokenMeDumpPolicy $parent 'test.exe' Enable)
    [void](Set-TokenMeDumpPolicy $parent 'test.exe' Disable)
    if ($parent.OpenSubKey('test.exe')) { throw 'Owned policy was not removed' }
    $custom = $parent.CreateSubKey('custom.exe')
    $custom.SetValue('DumpCount', 9); $custom.Dispose()
    if ((Set-TokenMeDumpPolicy $parent 'custom.exe' Enable).status -ne 'custom_policy_preserved') { throw 'Custom settings were overwritten' }
    if ((Set-TokenMeDumpPolicy $parent 'custom.exe' Disable).status -ne 'not_owned') { throw 'Custom settings were removed' }
    [void](Set-TokenMeDumpPolicy $parent 'edited.exe' Enable)
    $edited = $parent.OpenSubKey('edited.exe', $true); $edited.SetValue('DumpCount', 7); $edited.Dispose()
    if ((Set-TokenMeDumpPolicy $parent 'edited.exe' Enable).status -ne 'modified_policy_preserved') { throw 'Edited owned settings were overwritten' }
    if ((Set-TokenMeDumpPolicy $parent 'edited.exe' Disable).status -ne 'modified_policy_preserved') { throw 'Edited owned settings were removed' }
    Write-Output 'Crash dump policy checks passed: full dumps, per-user expansion, idempotence, cleanup and custom-policy preservation.'
} finally {
    $parent.Dispose()
    if (-not $testPath.StartsWith('Software\TokenMe\Tests\CrashPolicy-')) { throw 'Unexpected registry cleanup target' }
    $user.DeleteSubKeyTree($testPath, $false); $user.Dispose()
}
