[CmdletBinding()]
param(
    [switch]$Unsigned,
    [string]$SigningConfig,
    [string]$ToolkitPath = (Join-Path $PSScriptRoot '../src-tauri/target/cuda-12.9.1')
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
if (-not $IsWindows -or -not [Environment]::Is64BitProcess) { throw 'Use PowerShell 7 on Windows x64.' }
if ($Unsigned -and $SigningConfig) { throw 'Choose either -Unsigned or -SigningConfig.' }

# Initialize the supported VS 2022 C++ toolchain, including its redistributable directory.
$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio/Installer/vswhere.exe'
if (-not (Test-Path $vswhere)) { throw 'Install Visual Studio 2022 with Desktop development with C++.' }
$vs = & $vswhere -latest -version '[17.0,18.0)' -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
if (-not $vs) { throw 'Visual Studio 2022 C++ tools were not found.' }
& (Join-Path $vs 'Common7/Tools/Launch-VsDevShell.ps1') -Arch amd64 -HostArch amd64 -SkipAutomaticLocation
if (-not $env:VCToolsRedistDir) { throw 'MSVC redistributables were not found.' }

& (Join-Path $PSScriptRoot 'setup-windows-cuda.ps1') -Destination $ToolkitPath
# Ninja avoids requiring CUDA's machine-wide Visual Studio integration.
$env:CMAKE_GENERATOR = 'Ninja'
if (-not (Get-Command ninja -ErrorAction SilentlyContinue)) {
    $ninjaDirectory = Join-Path $vs 'Common7/IDE/CommonExtensions/Microsoft/CMake/Ninja'
    $env:PATH = $ninjaDirectory + [IO.Path]::PathSeparator + $env:PATH
}
if (-not (Get-Command ninja -ErrorAction SilentlyContinue)) { throw 'Ninja must be on PATH (included in the VS C++ CMake tools).' }
$buildArgs = @((Join-Path $PSScriptRoot 'windows-cuda.mjs'), 'build')
if ($Unsigned) { $buildArgs += '--unsigned' }
if ($SigningConfig) { $buildArgs += @('--config', [IO.Path]::GetFullPath($SigningConfig)) }
& node @buildArgs
if ($LASTEXITCODE -ne 0) { throw 'Windows CUDA build or runtime validation failed.' }

$root = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$version = (Get-Content (Join-Path $root 'package.json') -Raw | ConvertFrom-Json).version
$zipDirectory = Join-Path $root 'src-tauri/target/release/bundle/cuda-portable'
New-Item -ItemType Directory -Path $zipDirectory -Force | Out-Null
$zip = Join-Path $zipDirectory "Transcriptor-CUDA_${version}_windows-x64.zip"
Compress-Archive -Path (Join-Path $root 'src-tauri/target/windows-cuda/portable/*') -DestinationPath $zip -Force
Write-Host "Portable CUDA package: $zip"
