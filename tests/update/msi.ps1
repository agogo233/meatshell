param(
    [string]$WixBin,
    [string]$ScratchDirectory,
    [string]$RunDirectory
)
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot '../../src/update/install.ps1')

function Assert-True($Condition, [string]$Message) {
    if (-not $Condition) { throw $Message }
}

function Wait-ForFile([string]$Path, $Process, [int]$Seconds = 30) {
    $deadline = [DateTime]::UtcNow.AddSeconds($Seconds)
    while (-not (Test-Path -LiteralPath $Path)) {
        if ($Process.HasExited) { throw "Process exited before writing $Path" }
        if ([DateTime]::UtcNow -ge $deadline) { throw "Timed out waiting for $Path" }
        Start-Sleep -Milliseconds 100
    }
}

function Invoke-Msi([string]$Arguments) {
    $process = Start-Process -FilePath "$env:SystemRoot\System32\msiexec.exe" -ArgumentList $Arguments -WindowStyle Hidden -Wait -PassThru
    $code = $process.ExitCode
    $process.Dispose()
    return $code
}

if (-not $RunDirectory) {
    Assert-True ($WixBin -and $ScratchDirectory) 'Specify -WixBin and -ScratchDirectory.'
    $repo = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
    $run = Join-Path ([IO.Path]::GetFullPath($ScratchDirectory)) ([Guid]::NewGuid().ToString())
    [IO.Directory]::CreateDirectory($run) | Out-Null
    $productName = 'MeatShell Update Test ' + [IO.Path]::GetFileName($run).Substring(0, 8)
    $registryKey = 'Software\' + $productName
    $template = [IO.File]::ReadAllText((Join-Path $repo 'wix/main.wxs'))
    $template = $template.Replace('encoding="windows-1252"', 'encoding="utf-8"')
    $template = $template.Replace('Name="meatshell"', ('Name="' + $productName + '"'))
    $template = $template.Replace('Software\meatshell', $registryKey).Replace('meatshell.exe', 'update-fixture.exe')
    foreach ($guid in @('6E8F2A1B-3C4D-4E5F-9A0B-1C2D3E4F5A6B', 'ECB1F8D7-7CC7-5A8A-A7CC-56A267A502DC', 'E6877B59-AD5C-4CA3-A1F5-8F8E0CC6CC53', 'A034B952-E060-47C3-8D04-FCA147FBE3E5')) {
        $template = $template.Replace($guid, [Guid]::NewGuid().ToString().ToUpperInvariant())
    }
    $template = $template.Replace('SourceFile="assets\meatshell.ico"', ('SourceFile="' + [Security.SecurityElement]::Escape((Join-Path $repo 'assets/meatshell.ico')) + '"'))
    $template = $template.Replace('Source="THIRD_PARTY_NOTICES.md"', ('Source="' + [Security.SecurityElement]::Escape((Join-Path $repo 'THIRD_PARTY_NOTICES.md')) + '"'))
    $actions = Join-Path $run 'installer-actions.dll'
    & rustc --edition=2021 --crate-type cdylib -C opt-level=1 (Join-Path $PSScriptRoot 'installer_actions.rs') -o $actions
    Assert-True ($LASTEXITCODE -eq 0) 'Could not compile MSI actions.'
    $cases = @(
        @{ name = 'baseline'; version = '1.0.0'; action = ''; code = 0; installed = '1.0.0.0' },
        @{ name = 'success'; version = '1.0.1'; action = ''; code = 0; installed = '1.0.1.0' },
        @{ name = 'cancel'; version = '1.0.2'; action = 'CancelInstall'; code = 1602; installed = '1.0.1.0' },
        @{ name = 'failure'; version = '1.0.2'; action = 'FailInstall'; code = 1603; installed = '1.0.1.0' },
        @{ name = 'retry'; version = '1.0.2'; action = ''; code = 0; installed = '1.0.2.0' }
    )
    foreach ($case in $cases) {
        $case.product = '{' + [Guid]::NewGuid().ToString().ToUpperInvariant() + '}'
        $caseDir = Join-Path $run $case.name
        [IO.Directory]::CreateDirectory($caseDir) | Out-Null
        $assembly = Join-Path $caseDir 'version.cs'
        [IO.File]::WriteAllText($assembly, ('[assembly: System.Reflection.AssemblyVersion("' + $case.version + '.0")]' + "`n" + '[assembly: System.Reflection.AssemblyFileVersion("' + $case.version + '.0")]'))
        & "$env:SystemRoot\Microsoft.NET\Framework64\v4.0.30319\csc.exe" /nologo /target:winexe ("/out:" + (Join-Path $caseDir 'update-fixture.exe')) $assembly (Join-Path $PSScriptRoot 'fixture.cs')
        Assert-True ($LASTEXITCODE -eq 0) 'Could not compile fixture.'
        [xml]$wxs = $template
        $ns = New-Object Xml.XmlNamespaceManager($wxs.NameTable)
        $ns.AddNamespace('w', 'http://schemas.microsoft.com/wix/2006/wi')
        $product = $wxs.SelectSingleNode('/w:Wix/w:Product', $ns)
        $product.SetAttribute('Id', $case.product)
        if ($case.action) {
            $binary = $wxs.CreateElement('Binary', $product.NamespaceURI)
            $binary.SetAttribute('Id', 'TestActions')
            $binary.SetAttribute('SourceFile', $actions)
            $null = $product.AppendChild($binary)
            $action = $wxs.CreateElement('CustomAction', $product.NamespaceURI)
            foreach ($entry in @{ Id = 'TestOutcome'; BinaryKey = 'TestActions'; DllEntry = $case.action; Execute = 'deferred'; Return = 'check'; Impersonate = 'no' }.GetEnumerator()) {
                $action.SetAttribute($entry.Key, $entry.Value)
            }
            $null = $product.AppendChild($action)
            $sequence = $wxs.CreateElement('Custom', $product.NamespaceURI)
            $sequence.SetAttribute('Action', 'TestOutcome')
            $sequence.SetAttribute('After', 'InstallFiles')
            $sequence.InnerText = 'NOT REMOVE'
            $null = $product.SelectSingleNode('w:InstallExecuteSequence', $ns).AppendChild($sequence)
        }
        $source = Join-Path $caseDir 'main.wxs'
        $object = Join-Path $caseDir 'main.wixobj'
        $case.package = Join-Path $caseDir 'package.msi'
        $wxs.Save($source)
        & (Join-Path $WixBin 'candle.exe') -nologo -arch x64 -ext WixUtilExtension ("-dVersion=" + $case.version) ("-dCargoTargetBinDir=" + $caseDir) -out $object $source
        Assert-True ($LASTEXITCODE -eq 0) 'WiX compilation failed.'
        & (Join-Path $WixBin 'light.exe') -nologo -ext WixUIExtension -ext WixUtilExtension -out $case.package $object
        Assert-True ($LASTEXITCODE -eq 0) 'WiX linking failed.'
    }
    @{ name = $productName; registry = $registryKey; cases = $cases } | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (Join-Path $run 'manifest.json') -Encoding UTF8
    Write-Output "Test files: $run"
    # Elevate once for the isolated per-machine product and its final uninstall.
    $arguments = '-NoProfile -ExecutionPolicy Bypass -File ' + (Quote-NativeArgument $PSCommandPath) + ' -RunDirectory ' + (Quote-NativeArgument $run)
    $worker = Start-Process -FilePath "$env:SystemRoot\System32\WindowsPowerShell\v1.0\powershell.exe" -ArgumentList $arguments -Verb RunAs -WindowStyle Hidden -Wait -PassThru
    Assert-True ($worker.ExitCode -eq 0) "MSI test failed; see $run\result.json and runner.log."
    Get-Content -LiteralPath (Join-Path $run 'result.json') -Raw
    exit
}

$run = [IO.Path]::GetFullPath($RunDirectory)
$manifest = Get-Content -LiteralPath (Join-Path $run 'manifest.json') -Raw -Encoding UTF8 | ConvertFrom-Json
Start-Transcript -LiteralPath (Join-Path $run 'runner.log') | Out-Null
Add-Type @'
using System.Runtime.InteropServices;
public class InstallerState {
    [DllImport("msi.dll", CharSet = CharSet.Unicode)]
    public static extern int MsiQueryProductState(string product);
}
'@
$results = New-Object Collections.Generic.List[object]
$installed = Join-Path $run 'custom install with spaces'
$executable = Join-Path $installed 'update-fixture.exe'
$profile = Join-Path $run 'profile with spaces and 中文'
[IO.Directory]::CreateDirectory($profile) | Out-Null
$dataFile = Join-Path $profile 'settings.json'
[IO.File]::WriteAllText($dataFile, '{"savedConnection":"test-only","windowLayout":"preserve"}')
$dataHash = (Get-FileHash -LiteralPath $dataFile).Hash
$errorMessage = $null
$parent = $null
$helper = $null
try {
    $baseline = $manifest.cases[0]
    $arguments = '/i ' + (Quote-NativeArgument $baseline.package) + ' /qn /norestart APPLICATIONFOLDER=' + (Quote-NativeArgument $installed) + ' /l*v ' + (Quote-NativeArgument (Join-Path $run 'baseline/install.log'))
    Assert-True ((Invoke-Msi $arguments) -eq 0) 'Baseline installation failed.'
    Assert-True ([InstallerState]::MsiQueryProductState($baseline.product) -eq 5) 'Baseline registration missing.'
    $currentProduct = $baseline.product
    foreach ($case in $manifest.cases | Select-Object -Skip 1) {
        $evidence = Join-Path $run $case.name
        $stage = Join-Path $run ('updates\' + [Guid]::NewGuid().ToString())
        [IO.Directory]::CreateDirectory($stage) | Out-Null
        Copy-Item -LiteralPath $case.package -Destination (Join-Path $stage 'installer.msi')
        Copy-Item -LiteralPath (Join-Path $PSScriptRoot '../../src/update/install.ps1') -Destination (Join-Path $stage 'install.ps1')
        [IO.File]::WriteAllText((Join-Path $stage 'lease'), '')
        $exitSignal = Join-Path $evidence 'exit-parent'
        if (Test-Path -LiteralPath $exitSignal) { Remove-Item -LiteralPath $exitSignal }
        $receipt = Join-Path $profile 'restarted.txt'
        if (Test-Path -LiteralPath $receipt) { Remove-Item -LiteralPath $receipt }
        $parent = Start-Process -FilePath $executable -ArgumentList ('--wait-for ' + (Quote-NativeArgument $exitSignal)) -WindowStyle Hidden -PassThru
        @{
            parent_pid = $parent.Id; executable = $executable; data_dir = $profile
            sha256 = (Get-FileHash -LiteralPath (Join-Path $stage 'installer.msi')).Hash.ToLowerInvariant(); english = $true
        } | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $stage 'install.json') -Encoding UTF8
        $arguments = '-NoProfile -ExecutionPolicy Bypass -File ' + (Quote-NativeArgument (Join-Path $PSScriptRoot 'msi-helper.ps1')) + ' -UpdateDirectory ' + (Quote-NativeArgument $stage) + ' -EvidenceDirectory ' + (Quote-NativeArgument $evidence)
        $helper = Start-Process -FilePath "$env:SystemRoot\System32\WindowsPowerShell\v1.0\powershell.exe" -ArgumentList $arguments -WindowStyle Hidden -PassThru -RedirectStandardOutput (Join-Path $evidence 'helper.stdout') -RedirectStandardError (Join-Path $evidence 'helper.stderr')
        $null = $helper.Handle
        Wait-ForFile (Join-Path $stage 'ready') $helper
        [IO.File]::WriteAllText((Join-Path $stage 'go'), 'install')
        [IO.File]::WriteAllText($exitSignal, 'exit')
        Assert-True ($helper.WaitForExit(120000)) 'Installer helper timed out.'
        $helper.WaitForExit()
        Assert-True ($helper.ExitCode -eq 0) "Installer helper exited with $($helper.ExitCode)."
        $helper.Dispose(); $helper = $null
        $parent.WaitForExit(); $parent.Dispose(); $parent = $null
        $code = [int](Get-Content -LiteralPath (Join-Path $evidence 'exit-code.txt') -Raw)
        Assert-True ($code -eq $case.code) "Unexpected MSI exit code for $($case.name): $code"
        $deadline = [DateTime]::UtcNow.AddSeconds(15)
        while (-not (Test-Path -LiteralPath $receipt) -and [DateTime]::UtcNow -lt $deadline) { Start-Sleep -Milliseconds 100 }
        $version = [Diagnostics.FileVersionInfo]::GetVersionInfo($executable).FileVersion
        Assert-True ($version -eq $case.installed) 'Installed version was not replaced or rolled back correctly.'
        $restartedVersion = [IO.File]::ReadAllText($receipt)
        Assert-True ($restartedVersion -eq $case.installed) 'Wrong version restarted or data directory changed.'
        Assert-True ((Get-FileHash -LiteralPath $dataFile).Hash -eq $dataHash) 'Profile data changed.'
        Assert-True (-not (Test-Path -LiteralPath $stage)) 'Temporary update files remain.'
        if ($case.code -eq 0) {
            Assert-True ([InstallerState]::MsiQueryProductState($currentProduct) -eq -1) 'Previous product registration remains.'
            $currentProduct = $case.product
        } else {
            Assert-True ([InstallerState]::MsiQueryProductState($case.product) -eq -1) 'Failed upgrade remains registered.'
            Assert-True (Test-Path -LiteralPath (Join-Path $evidence 'message.txt')) 'Failure did not report a message.'
            $log = Get-Content -LiteralPath (Join-Path $evidence 'install.log') -Raw
            Assert-True ($log -match 'FileCopy\(SourceName=[^\r\n]+\.rbf,,DestName=[^\r\n]+update-fixture\.exe,') 'MSI did not restore the executable from its rollback backup.'
        }
        Assert-True ([InstallerState]::MsiQueryProductState($currentProduct) -eq 5) 'Installed product registration missing.'
        $location = (Get-ItemProperty -LiteralPath ('HKLM:\' + $manifest.registry)).InstallDir
        Assert-True ($location.TrimEnd('\') -eq $installed) 'Custom installation directory changed.'
        $results.Add([PSCustomObject]@{
            scenario = $case.name; msi_exit_code = $code
            installed_version = $version; restarted_version = $restartedVersion
            profile_preserved = $true; temporary_directory_removed = $true; stage = $stage
        })
        Write-Output "PASS: $($case.name) ($code, $version)"
    }
} catch {
    $errorMessage = $_.ToString() + "`n" + $_.ScriptStackTrace
    Write-Output $errorMessage
} finally {
    if ($parent -and -not $parent.HasExited) { $parent.Kill(); $parent.WaitForExit() }
    # Do not interrupt Windows Installer. A timed-out worker retains evidence.
    if ($helper -and -not $helper.HasExited) { $helper.WaitForExit() }
    foreach ($case in $manifest.cases) {
        if ([InstallerState]::MsiQueryProductState($case.product) -eq 5) {
            $code = Invoke-Msi ('/x ' + $case.product + ' /qn /norestart /l*v ' + (Quote-NativeArgument (Join-Path $run ('uninstall-' + $case.name + '.log'))))
            if ($code -ne 0) { $errorMessage += " Uninstall failed: $code" }
        }
    }
    $uninstalled = -not (Test-Path -LiteralPath $executable)
    foreach ($case in $manifest.cases) { $uninstalled = $uninstalled -and ([InstallerState]::MsiQueryProductState($case.product) -eq -1) }
    if (-not $uninstalled) { $errorMessage += ' Test product cleanup failed.' }
    @{ passed = -not $errorMessage; error = $errorMessage; scenarios = @($results.ToArray()); test_product_uninstalled = $uninstalled; run_directory = $run } | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (Join-Path $run 'result.json') -Encoding UTF8
    Stop-Transcript | Out-Null
}
if ($errorMessage) { exit 1 }
