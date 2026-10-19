param(
    [Parameter(Mandatory = $true)][string]$UpdateDirectory,
    [Parameter(Mandatory = $true)][string]$EvidenceDirectory
)
$ErrorActionPreference = 'Stop'
. (Join-Path $UpdateDirectory 'install.ps1')

# Capture messages without leaving a modal dialog open in an unattended test.
function Show-UpdateMessage([string]$Message) {
    [IO.File]::WriteAllText((Join-Path $EvidenceDirectory 'message.txt'), $Message)
}

# Delegate every launch to the real Windows process API; only add MSI logging.
function Start-Process {
    param($FilePath, $ArgumentList, $Verb, $WindowStyle, [switch]$Wait, [switch]$PassThru)
    $parameters = @{} + $PSBoundParameters
    if ($FilePath.EndsWith('msiexec.exe')) {
        $parameters.ArgumentList += ' /l*v ' + (Quote-NativeArgument (Join-Path $EvidenceDirectory 'install.log'))
        $process = Microsoft.PowerShell.Management\Start-Process @parameters
        [IO.File]::WriteAllText((Join-Path $EvidenceDirectory 'exit-code.txt'), [string]$process.ExitCode)
        return $process
    }
    Microsoft.PowerShell.Management\Start-Process @parameters
}

Invoke-MeatShellUpdate $UpdateDirectory
