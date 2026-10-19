function Quote-NativeArgument([string]$Value) {
    '"' + [regex]::Replace([regex]::Replace($Value, '(\\*)"', '$1$1\"'), '(\\+)$', '$1$1') + '"'
}

function Show-UpdateMessage([string]$Message) {
    Add-Type -AssemblyName System.Windows.Forms
    [Windows.Forms.MessageBox]::Show($Message, 'MeatShell Update') | Out-Null
}

function Invoke-MeatShellUpdate([string]$UpdateDirectory) {
    # Runs outside the GUI so Windows Installer can replace the application.
    $ErrorActionPreference = 'Stop'
    $updateDir = [IO.Path]::GetFullPath($UpdateDirectory)
    $lease = $null
    $packageStream = $null
    $installRequested = $false
    $parentExited = $false
    $message = $null
    $plan = $null

    try {
        $lease = [IO.File]::Open((Join-Path $updateDir 'lease'), 'Open', 'Read', 'Read')
        $plan = Get-Content -LiteralPath (Join-Path $updateDir 'install.json') -Raw -Encoding UTF8 | ConvertFrom-Json
        $parent = [Diagnostics.Process]::GetProcessById($plan.parent_pid)
        # Acquiring the handle before acknowledging readiness avoids a PID reuse race.
        $null = $parent.Handle
        $package = Join-Path $updateDir 'installer.msi'
        $packageStream = [IO.File]::Open($package, 'Open', 'Read', 'Read')
        $sha = [Security.Cryptography.SHA256]::Create()
        try {
            $actual = [BitConverter]::ToString($sha.ComputeHash($packageStream)).Replace('-', '').ToLowerInvariant()
        } finally { $sha.Dispose() }
        if ($actual -ne $plan.sha256) { throw 'Installer checksum mismatch.' }

        [IO.File]::WriteAllText((Join-Path $updateDir 'ready'), 'ready')
        $deadline = [DateTime]::UtcNow.AddSeconds(30)
        while (-not (Test-Path -LiteralPath (Join-Path $updateDir 'go'))) {
            if ([DateTime]::UtcNow -ge $deadline) { throw 'The application did not confirm installation.' }
            Start-Sleep -Milliseconds 100
        }
        $installRequested = $true
        if (-not $parent.WaitForExit(60000)) { throw 'MeatShell did not exit. Please close it and retry.' }
        $parentExited = $true
        $installer = Join-Path $env:SystemRoot 'System32\msiexec.exe'
        $arguments = '/i ' + (Quote-NativeArgument $package) + ' /passive /norestart'
        $process = Start-Process -FilePath $installer -ArgumentList $arguments -Verb RunAs -WindowStyle Hidden -Wait -PassThru
        if ($process.ExitCode -eq 3010) {
            $message = if ($plan.english) { 'Update installed. Windows needs a restart to finish replacing files.' } else { '更新已安装。Windows 需要重启以完成文件替换。' }
        } elseif ($process.ExitCode -ne 0) {
            throw "Windows Installer returned $($process.ExitCode)."
        }
    } catch {
        $detail = $_.Exception.Message
        if (-not $installRequested) {
            [IO.File]::WriteAllText((Join-Path $updateDir 'error'), $detail)
        }
        $message = if ($plan.english) {
            "The update did not finish. You can retry from Settings. The installed application will be reopened if available.`n`n$detail"
        } else {
            "更新未完成，可在设置中重试。程序将尝试重新打开当前安装版本。`n`n$detail"
        }
    } finally {
        if ($packageStream) { $packageStream.Dispose() }
        if ($lease) { $lease.Dispose() }
        # Delete only this attempt's known files. Never clear Windows Installer's cache.
        if ($installRequested) {
            foreach ($name in @('installer.part', 'installer.msi', 'install.json', 'ready', 'go', 'error', 'install.ps1', 'lease')) {
                $file = Join-Path $updateDir $name
                for ($attempt = 0; $attempt -lt 3; $attempt++) {
                    try {
                        if (Test-Path -LiteralPath $file) { Remove-Item -LiteralPath $file -Force }
                        break
                    } catch { Start-Sleep -Milliseconds 300 }
                }
            }
            # Non-recursive removal leaves unexpected files untouched.
            try { [IO.Directory]::Delete($updateDir) } catch { }
        }
    }

    if (-not $installRequested) { return }
    if ($message) {
        Show-UpdateMessage $message
    }
    if ($parentExited -and (Test-Path -LiteralPath $plan.executable)) {
        $arguments = '--data-dir ' + (Quote-NativeArgument $plan.data_dir)
        try {
            Start-Process -FilePath $plan.executable -ArgumentList $arguments -WindowStyle Normal
        } catch {
            $detail = $_.Exception.Message
            $message = if ($plan.english) {
                "Could not reopen MeatShell. Please start it from its shortcut.`n`n$detail"
            } else {
                "无法重新打开 MeatShell，请从快捷方式启动。`n`n$detail"
            }
            Show-UpdateMessage $message
        }
    }

}

if ($MyInvocation.InvocationName -ne '.') {
    Invoke-MeatShellUpdate $PSScriptRoot
}
