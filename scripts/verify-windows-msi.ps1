param(
    [Parameter(Mandatory = $true)][string]$CandidateMsi,
    [Parameter(Mandatory = $true)][string]$SourceExe,
    [string]$CandidateVersion = '0.7.6'
)

$ErrorActionPreference = 'Stop'
$CandidateMsi = (Resolve-Path $CandidateMsi).Path
$SourceExe = (Resolve-Path $SourceExe).Path
$defaultDir = Join-Path $env:ProgramFiles 'Probe Shell'
$startMenuDir = Join-Path $env:ProgramData 'Microsoft\Windows\Start Menu\Programs\Probe Shell'
$shortcutPath = Join-Path $startMenuDir 'Probe Shell.lnk'

Add-Type -TypeDefinition @'
using System.Text;
using System.Runtime.InteropServices;

public static class WindowsInstallerProductInfo
{
    [DllImport("msi.dll", CharSet = CharSet.Unicode, EntryPoint = "MsiGetProductInfoW")]
    public static extern uint MsiGetProductInfo(
        string productCode,
        string attribute,
        StringBuilder valueBuffer,
        ref uint valueBufferLength);
}
'@

function Get-MsiProductInfo([string]$ProductCode, [string]$Attribute) {
    $length = [uint32]4096
    $buffer = [Text.StringBuilder]::new([int]$length)
    $rc = [WindowsInstallerProductInfo]::MsiGetProductInfo($ProductCode, $Attribute, $buffer, [ref]$length)
    if ($rc -ne 0) {
        throw "MsiGetProductInfo('$Attribute') failed with Win32 error $rc for $ProductCode"
    }
    return $buffer.ToString()
}

function Invoke-Msi([string]$Arguments, [string]$LogName) {
    $log = Join-Path $env:RUNNER_TEMP $LogName
    $psi = [System.Diagnostics.ProcessStartInfo]::new()
    $psi.FileName = 'msiexec.exe'
    $psi.Arguments = "$Arguments /l*v `"$log`""
    $psi.UseShellExecute = $false
    $psi.CreateNoWindow = $true
    $p = [System.Diagnostics.Process]::Start($psi)
    if (-not $p.WaitForExit(120000)) {
        try { $p.Kill($true) } catch {}
        if (Test-Path $log) { Get-Content $log -Tail 160 }
        throw "msiexec timed out: $Arguments"
    }
    if ($p.ExitCode -notin @(0, 3010)) {
        if (Test-Path $log) { Get-Content $log -Tail 200 }
        throw "msiexec failed with exit code $($p.ExitCode): $Arguments"
    }
}

function Get-ProbeEntries {
    $roots = @(
        'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\*',
        'HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\*'
    )
    return @($roots | ForEach-Object { Get-ItemProperty $_ -ErrorAction SilentlyContinue } | Where-Object { $_.DisplayName -eq 'Probe Shell' })
}

function Assert-StartMenuShortcut([string]$ExpectedExe) {
    if (-not (Test-Path $shortcutPath)) {
        throw "Start Menu shortcut is missing: $shortcutPath"
    }
    $shortcutFile = Get-Item $shortcutPath
    if ($shortcutFile.Length -lt 100) {
        throw "Start Menu shortcut is unexpectedly small/corrupt: $($shortcutFile.Length) bytes"
    }

    # Advertised MSI shortcuts intentionally may not expose a normal TargetPath
    # through WScript.Shell. If a normal target is exposed, it must still point
    # at the installed executable. WiX ICE validation verifies the advertised
    # component/feature relationship during light.exe linking.
    $wsh = New-Object -ComObject WScript.Shell
    $shortcut = $wsh.CreateShortcut($shortcutPath)
    if (-not [string]::IsNullOrWhiteSpace([string]$shortcut.TargetPath)) {
        $actual = ([IO.Path]::GetFullPath($shortcut.TargetPath)).TrimEnd('\')
        $expected = ([IO.Path]::GetFullPath($ExpectedExe)).TrimEnd('\')
        if ($actual -ne $expected) {
            throw "Start Menu shortcut targets '$actual' instead of '$expected'"
        }
    }
}

function Assert-Clean([string]$InstallDir) {
    if (Test-Path (Join-Path $InstallDir 'probe-shell.exe')) {
        throw "Executable remains after uninstall: $InstallDir"
    }
    if (Test-Path $startMenuDir) {
        throw "Start Menu directory remains after uninstall: $startMenuDir"
    }
    $entries = @(Get-ProbeEntries)
    if ($entries.Count -ne 0) {
        throw "Probe Shell uninstall entry remains: $($entries.DisplayVersion -join ', ')"
    }
}

function Assert-Installed([string]$InstallDir, [string]$ExpectedVersion) {
    $exe = Join-Path $InstallDir 'probe-shell.exe'
    if (-not (Test-Path $exe)) {
        throw "Installed executable is missing: $exe"
    }

    $files = @(Get-ChildItem $InstallDir -Recurse -File)
    if ($files.Count -ne 1 -or $files[0].Name -ne 'probe-shell.exe') {
        throw "Install directory must contain only probe-shell.exe; found: $($files.FullName -join ', ')"
    }

    $sourceHash = (Get-FileHash $SourceExe -Algorithm SHA256).Hash
    $installedHash = (Get-FileHash $exe -Algorithm SHA256).Hash
    if ($sourceHash -ne $installedHash) {
        throw 'Installed EXE does not match the source release executable'
    }

    Assert-StartMenuShortcut $exe

    $entries = @(Get-ProbeEntries)
    if ($entries.Count -ne 1) {
        throw "Expected exactly one Apps & Features entry; found $($entries.Count)"
    }
    $entry = $entries[0]
    if ($entry.DisplayVersion -ne $ExpectedVersion) {
        throw "Expected DisplayVersion $ExpectedVersion, got $($entry.DisplayVersion)"
    }
    if ($entry.Publisher -ne 'OnlyChallgener') {
        throw "Expected Publisher OnlyChallgener, got '$($entry.Publisher)'"
    }

    $productIcon = Get-MsiProductInfo $entry.PSChildName 'ProductIcon'
    if ([string]::IsNullOrWhiteSpace($productIcon)) {
        throw 'Windows Installer ProductIcon is missing'
    }
    if (-not (Test-Path $productIcon)) {
        throw "Windows Installer ProductIcon path does not exist: $productIcon"
    }

    if ([string]::IsNullOrWhiteSpace([string]$entry.URLInfoAbout)) {
        throw 'Apps & Features project URL is missing'
    }
    if ([string]::IsNullOrWhiteSpace([string]$entry.InstallLocation)) {
        throw 'Apps & Features InstallLocation is missing'
    }

    $actualLocation = ([IO.Path]::GetFullPath([string]$entry.InstallLocation)).TrimEnd('\')
    $expectedLocation = ([IO.Path]::GetFullPath($InstallDir)).TrimEnd('\')
    if ($actualLocation -ne $expectedLocation) {
        throw "InstallLocation '$actualLocation' does not match '$expectedLocation'"
    }
}

function Remove-AnyProbeShell {
    foreach ($entry in @(Get-ProbeEntries)) {
        if ($entry.PSChildName -match '^\{[0-9A-Fa-f-]+\}$') {
            Invoke-Msi "/x $($entry.PSChildName) /qn /norestart" 'cleanup-existing.log'
        }
    }
}

Remove-AnyProbeShell
Assert-Clean $defaultDir

# 1. Fresh default installation.
Invoke-Msi "/i `"$CandidateMsi`" /qn /norestart" 'fresh-install.log'
Assert-Installed $defaultDir $CandidateVersion
Invoke-Msi "/x `"$CandidateMsi`" /qn /norestart" 'fresh-uninstall.log'
Assert-Clean $defaultDir

# 2. Custom installation directory. APPLICATIONFOLDER is also the WixUI Browse target.
$customDir = Join-Path $env:RUNNER_TEMP 'Probe Shell Custom Install'
Invoke-Msi "/i `"$CandidateMsi`" /qn /norestart APPLICATIONFOLDER=`"$customDir`"" 'custom-install.log'
Assert-Installed $customDir $CandidateVersion
Invoke-Msi "/x `"$CandidateMsi`" /qn /norestart" 'custom-uninstall.log'
Assert-Clean $customDir

# 3. Upgrade actual published MSI packages, not locally reconstructed surrogates.
$oldPackages = @(
    @{ Version = '0.7.3'; Url = 'https://github.com/OnlyChallgener/probe-shell/releases/download/v0.7.3/probe-shell-v0.7.3-windows-x86_64.msi' },
    @{ Version = '0.7.5'; Url = 'https://github.com/OnlyChallgener/probe-shell/releases/download/v0.7.5/probe-shell-v0.7.5-windows-x86_64.msi' }
)

foreach ($old in $oldPackages) {
    $oldMsi = Join-Path $env:RUNNER_TEMP "probe-shell-v$($old.Version).msi"
    Invoke-WebRequest -UseBasicParsing -Uri $old.Url -OutFile $oldMsi
    Invoke-Msi "/i `"$oldMsi`" /qn /norestart" "install-old-$($old.Version).log"

    $before = @(Get-ProbeEntries)
    if ($before.Count -ne 1 -or $before[0].DisplayVersion -ne $old.Version) {
        throw "Published v$($old.Version) MSI did not register as expected"
    }

    Invoke-Msi "/i `"$CandidateMsi`" /qn /norestart" "upgrade-from-$($old.Version).log"
    Assert-Installed $defaultDir $CandidateVersion
    if (@(Get-ProbeEntries).Count -ne 1) {
        throw "Upgrade from v$($old.Version) left multiple Probe Shell installations"
    }

    Invoke-Msi "/x `"$CandidateMsi`" /qn /norestart" "uninstall-after-$($old.Version).log"
    Assert-Clean $defaultDir
}

Write-Host 'Windows MSI installer verification passed.'
