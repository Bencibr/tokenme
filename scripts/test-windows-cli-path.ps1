$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'windows-cli-path.ps1') -Mode Add -Scope User -Directory 'C:\Program Files\TokenMe\bin'

function Assert-Path([string]$Actual, [string]$Expected) {
    if ($Actual -cne $Expected) { throw "PATH mismatch: expected [$Expected], got [$Actual]" }
}

$bin = 'C:\Program Files\TokenMe\bin'
Assert-Path (Update-TokenMePath '' $bin Add) $bin
$original = '%SystemRoot%\system32;C:\Users\demo\other;'
$added = Update-TokenMePath $original $bin Add
Assert-Path $added ($original + ';' + $bin)
Assert-Path (Update-TokenMePath $added $bin Add) $added
Assert-Path (Update-TokenMePath $added $bin Remove) $original
Assert-Path (Update-TokenMePath ('C:\Users\demo\other;"c:\PROGRAM FILES\TokenMe\bin\";C:\Users\demo\last') $bin Add) 'C:\Users\demo\other;"c:\PROGRAM FILES\TokenMe\bin\";C:\Users\demo\last'
Assert-Path (Update-TokenMePath ('C:\Users\demo\other;"c:\PROGRAM FILES\TokenMe\bin\";C:\Users\demo\last') $bin Remove) 'C:\Users\demo\other;C:\Users\demo\last'
Assert-Path (Update-TokenMePath "$bin;$bin" $bin Remove) ''
Assert-Path (Update-TokenMePath 'C:\Program Files\TokenMe\bin-other;C:\Users\demo\other' $bin Remove) 'C:\Program Files\TokenMe\bin-other;C:\Users\demo\other'
Assert-Path (Update-TokenMePath 'C:\Users\demo\other;;C:\Users\demo\last;' $bin Remove) 'C:\Users\demo\other;;C:\Users\demo\last;'

# Exercise a PATH longer than NSIS's string buffer and keep variable references.
$longPath = ('%SystemRoot%\system32;' * 1000).TrimEnd(';')
Assert-Path (Update-TokenMePath (Update-TokenMePath $longPath $bin Add) $bin Remove) $longPath
$expanded = Join-Path $env:LOCALAPPDATA 'TokenMe\bin'
Assert-Path (Update-TokenMePath '%LOCALAPPDATA%\TokenMe\bin;C:\Users\demo\other' $expanded Add) '%LOCALAPPDATA%\TokenMe\bin;C:\Users\demo\other'
Assert-Path (Update-TokenMePath '%LOCALAPPDATA%\TokenMe\bin;C:\Users\demo\other' $expanded Remove) 'C:\Users\demo\other'
Write-Output 'Windows CLI PATH checks passed: idempotence, exact removal, quoted paths, variables, empty entries and long PATH.'
