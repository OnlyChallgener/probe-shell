param(
    [Parameter(Mandatory = $true)][string]$Version,
    [Parameter(Mandatory = $true)][string]$CargoTargetBinDir,
    [Parameter(Mandatory = $true)][string]$OutputMsi
)

$ErrorActionPreference = 'Stop'
$repoRoot = Split-Path -Parent $PSScriptRoot
$wixSource = Join-Path $repoRoot 'wix\main.wxs'
$iconSource = Join-Path $repoRoot 'assets\probe-shell.ico'
$exeSource = Join-Path $CargoTargetBinDir 'probe-shell.exe'

foreach ($required in @($wixSource, $iconSource, $exeSource)) {
    if (-not (Test-Path $required)) {
        throw "Required MSI input is missing: $required"
    }
}

choco install wixtoolset -y --no-progress
$wixRoot = Get-ChildItem 'C:\Program Files (x86)' -Directory -Filter 'WiX Toolset v3.*' -ErrorAction SilentlyContinue |
    Sort-Object Name -Descending |
    Select-Object -First 1
if (-not $wixRoot) {
    throw 'WiX Toolset v3 install directory was not found'
}

$wixBin = Join-Path $wixRoot.FullName 'bin'
$candle = Join-Path $wixBin 'candle.exe'
$light = Join-Path $wixBin 'light.exe'
$dark = Join-Path $wixBin 'dark.exe'
foreach ($tool in @($candle, $light, $dark)) {
    if (-not (Test-Path $tool)) {
        throw "WiX tool was not found: $tool"
    }
}

$outputPath = [IO.Path]::GetFullPath($OutputMsi)
$outputDir = Split-Path -Parent $outputPath
New-Item -ItemType Directory -Force -Path $outputDir | Out-Null

$workDir = Join-Path $env:RUNNER_TEMP ('probe-shell-msi-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Force -Path $workDir | Out-Null
try {
    $wixObj = Join-Path $workDir 'probe-shell.wixobj'
    & $candle -nologo -arch x64 "-dVersion=$Version" "-dCargoTargetBinDir=$CargoTargetBinDir" -out $wixObj $wixSource
    if ($LASTEXITCODE -ne 0) {
        throw "candle failed: $LASTEXITCODE"
    }

    & $light -nologo -ext WixUIExtension -out $outputPath $wixObj
    if ($LASTEXITCODE -ne 0) {
        throw "light failed: $LASTEXITCODE"
    }

    # WiX writes a sibling .wixpdb by default. It is build/debug metadata and is
    # not a release artifact.
    $wixPdb = [IO.Path]::ChangeExtension($outputPath, '.wixpdb')
    Remove-Item $wixPdb -Force -ErrorAction SilentlyContinue

    # Decompile/extract the produced MSI and fail if a ZIP payload somehow enters
    # the installer. The MSI is expected to carry only its normal CAB payload
    # containing probe-shell.exe plus MSI metadata/UI resources.
    $extractDir = Join-Path $workDir 'extracted'
    $decompiled = Join-Path $workDir 'decompiled.wxs'
    New-Item -ItemType Directory -Force -Path $extractDir | Out-Null
    & $dark -nologo -x $extractDir $outputPath -o $decompiled
    if ($LASTEXITCODE -ne 0) {
        throw "dark failed: $LASTEXITCODE"
    }

    $embeddedZip = @(Get-ChildItem $extractDir -Recurse -File -Filter '*.zip' -ErrorAction SilentlyContinue)
    if ($embeddedZip.Count -ne 0) {
        throw "MSI unexpectedly contains ZIP payloads: $($embeddedZip.FullName -join ', ')"
    }

    if (-not (Test-Path $outputPath)) {
        throw "MSI was not produced: $outputPath"
    }
    Write-Host "Built and inspected MSI: $outputPath"
}
finally {
    Remove-Item $workDir -Recurse -Force -ErrorAction SilentlyContinue
}
