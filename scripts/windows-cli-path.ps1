[CmdletBinding()]
param(
    [Parameter(Mandatory)][ValidateSet('Add', 'Remove')][string]$Mode,
    [Parameter(Mandatory)][ValidateSet('User', 'Machine')][string]$Scope,
    [Parameter(Mandatory)][string]$Directory
)

$ErrorActionPreference = 'Stop'

function Get-TokenMePathEntry([string]$Entry) {
    [Environment]::ExpandEnvironmentVariables($Entry.Trim().Trim('"')).TrimEnd('\', '/')
}

function Update-TokenMePath([string]$Value, [string]$Bin, [string]$Action) {
    $normalized = Get-TokenMePathEntry $Bin
    $parts = if ($Value.Length) { $Value.Split(';') } else { @() }
    $matching = @($parts | Where-Object { (Get-TokenMePathEntry $_) -ieq $normalized })
    if ($Action -eq 'Add') {
        if ($matching.Count) { return $Value }
        if (-not $Value.Length) { return $Bin }
        return "$Value;$Bin"
    }
    # Preserve every other entry verbatim, including unexpanded variables.
    return (@($parts | Where-Object { (Get-TokenMePathEntry $_) -ine $normalized }) -join ';')
}

# Dot sourcing exposes the pure transformation for focused checks.
if ($MyInvocation.InvocationName -eq '.') { return }

try {
    if (-not [IO.Path]::IsPathRooted($Directory) -or $Directory.Contains(';')) {
        throw 'The CLI directory must be an absolute path without semicolons.'
    }
    $Directory = [IO.Path]::GetFullPath($Directory).TrimEnd('\', '/')
    if ($Mode -eq 'Add' -and -not (Test-Path -LiteralPath (Join-Path $Directory 'tokenme.exe'))) {
        throw 'The CLI binary is missing; PATH was not changed.'
    }
    $hive = if ($Scope -eq 'Machine') { [Microsoft.Win32.Registry]::LocalMachine } else { [Microsoft.Win32.Registry]::CurrentUser }
    $keyPath = if ($Scope -eq 'Machine') { 'SYSTEM\CurrentControlSet\Control\Session Manager\Environment' } else { 'Environment' }
    $key = $hive.CreateSubKey($keyPath)
    try {
        $old = [string]$key.GetValue('Path', '', [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
        $new = Update-TokenMePath $old $Directory $Mode
        if ($new -ceq $old) { exit 0 }
        $kind = if ($null -ne $key.GetValue('Path')) { $key.GetValueKind('Path') } else { [Microsoft.Win32.RegistryValueKind]::ExpandString }
        if ($new.Length) { $key.SetValue('Path', $new, $kind) }
        else { $key.DeleteValue('Path', $false) }
    }
    finally { $key.Dispose() }

    # Notify Explorer so new terminals inherit the updated environment.
    Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class TokenMeEnvironment {
    [DllImport("user32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    public static extern IntPtr SendMessageTimeout(IntPtr window, uint message,
        UIntPtr wParam, string lParam, uint flags, uint timeout, out UIntPtr result);
}
'@
    $messageResult = [UIntPtr]::Zero
    [void][TokenMeEnvironment]::SendMessageTimeout([IntPtr]0xffff, 0x1a, [UIntPtr]::Zero, 'Environment', 2, 2000, [ref]$messageResult)
}
catch {
    Write-Error $_
    exit 1
}
