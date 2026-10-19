param([Parameter(Mandatory = $true)][string]$ScratchDirectory)
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot '../../src/update/install.ps1')

function Assert-True($Condition, [string]$Message) {
    if (-not $Condition) { throw $Message }
}

function Show-UpdateMessage([string]$Message) { $script:reportedMessage = $Message }

# These mocks exercise the real handoff and cleanup without running an MSI or GUI.
function Start-Process {
    param($FilePath, $ArgumentList, $Verb, $WindowStyle, [switch]$Wait, [switch]$PassThru)
    if ($FilePath.EndsWith('msiexec.exe')) {
        $script:installCalls++
        Assert-True $Wait 'Installer must finish before cleanup'
        Assert-True ($Verb -eq 'RunAs') 'Installation must request elevation'
        Assert-True ($ArgumentList.Contains('/norestart')) 'Installer must not reboot Windows'
        Assert-True (Test-Path -LiteralPath (Join-Path $script:attemptDir 'installer.msi')) 'Package deleted before installation'
        if ($script:installerResult -eq 'uac-cancel') { throw 'The operation was canceled by the user.' }
        return [PSCustomObject]@{ ExitCode = [int]$script:installerResult }
    }
    $script:restartCalls++
    Assert-True (-not (Test-Path -LiteralPath $script:attemptDir)) 'Temporary package must be cleaned before restart'
    Assert-True ($ArgumentList -eq ('--data-dir ' + (Quote-NativeArgument $script:profileDir))) 'Restart must preserve the selected profile'
}

$scratch = [IO.Path]::GetFullPath($ScratchDirectory)
[IO.Directory]::CreateDirectory($scratch) | Out-Null
$fakeExe = Join-Path $scratch 'installed app.exe'
[IO.File]::WriteAllText($fakeExe, 'never executed')
$script:profileDir = Join-Path $scratch 'profile with spaces and 中文'

try {
    foreach ($outcome in @('0', '3010', '1602', '1603', 'uac-cancel', 'bad-checksum')) {
        $script:attemptDir = Join-Path $scratch ([Guid]::NewGuid().ToString())
        [IO.Directory]::CreateDirectory($script:attemptDir) | Out-Null
        [IO.File]::WriteAllText((Join-Path $script:attemptDir 'lease'), '')
        $package = Join-Path $script:attemptDir 'installer.msi'
        [IO.File]::WriteAllText($package, 'synthetic package')
        $digest = (Get-FileHash -LiteralPath $package -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($outcome -eq 'bad-checksum') { $digest = '0' * 64 }
        $parent = Microsoft.PowerShell.Management\Start-Process -FilePath (Join-Path $PSHOME 'powershell.exe') -ArgumentList '-NoProfile -Command Start-Sleep -Seconds 1' -WindowStyle Hidden -PassThru
        @{
            parent_pid = $parent.Id; executable = $fakeExe; data_dir = $script:profileDir
            sha256 = $digest; english = $true
        } | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $script:attemptDir 'install.json') -Encoding UTF8
        [IO.File]::WriteAllText((Join-Path $script:attemptDir 'go'), 'install')
        $script:installerResult = $outcome
        $script:installCalls = 0
        $script:restartCalls = 0
        $script:reportedMessage = $null
        Invoke-MeatShellUpdate $script:attemptDir
        $parent.WaitForExit()
        $parent.Dispose()
        if ($outcome -eq 'bad-checksum') {
            Assert-True ($script:installCalls -eq 0) 'A corrupted package must never be installed'
            Assert-True ($script:restartCalls -eq 0) 'Failed preparation must leave the running GUI alone'
            Assert-True (-not (Test-Path -LiteralPath (Join-Path $script:attemptDir 'ready'))) 'Corrupt package was acknowledged'
            Assert-True ((Get-Content -LiteralPath (Join-Path $script:attemptDir 'error') -Raw).Contains('checksum')) 'Missing preparation error'
            foreach ($name in @('installer.msi', 'lease', 'install.json', 'go', 'error')) {
                Remove-Item -LiteralPath (Join-Path $script:attemptDir $name)
            }
            [IO.Directory]::Delete($script:attemptDir)
        } else {
            Assert-True ($script:installCalls -eq 1) 'Installer should run exactly once'
            Assert-True ($script:restartCalls -eq 1) 'Installed application should reopen after success or failure'
            Assert-True (-not (Test-Path -LiteralPath $script:attemptDir)) 'Temporary files remain'
            Assert-True (($outcome -eq '0') -eq ($null -eq $script:reportedMessage)) 'Unexpected installation status message'
        }
        Write-Output "PASS: $outcome"
    }
    Assert-True ((Quote-NativeArgument 'C:\a b\') -eq '"C:\a b\\"') 'Trailing path separator must be escaped'
    Assert-True ((Quote-NativeArgument 'C:\a"b') -eq '"C:\a\"b"') 'Embedded quote must be escaped'
    Write-Output 'PASS: native argument quoting'
} finally {
    Remove-Item -LiteralPath $fakeExe -ErrorAction SilentlyContinue
}
