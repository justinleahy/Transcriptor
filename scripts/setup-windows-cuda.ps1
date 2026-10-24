# Download only build/runtime components. No installer, driver, registry or machine PATH changes.
[CmdletBinding()]
param([Parameter(Mandatory)][string]$Destination)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$lockPath = Join-Path $PSScriptRoot 'cuda-windows.lock.json'
$lock = Get-Content $lockPath -Raw | ConvertFrom-Json
$lockHash = (Get-FileHash $lockPath -Algorithm SHA256).Hash
$Destination = [IO.Path]::GetFullPath($Destination)
$marker = Join-Path $Destination 'transcriptor-cuda-lock.txt'
$required = @('bin/nvcc.exe', 'bin/cudart64_12.dll', 'bin/cublas64_12.dll', 'bin/cublasLt64_12.dll',
    'include/cuda_runtime.h', 'include/cublas_v2.h', 'include/crt/host_config.h',
    'lib/x64/cuda.lib', 'lib/x64/cudart.lib', 'lib/x64/cublas.lib', 'lib/x64/cublasLt.lib', 'nvvm/libdevice')

function Assert-Toolkit([string]$Root) {
    foreach ($path in $required) {
        if (-not (Test-Path (Join-Path $Root $path))) { throw "CUDA component missing: $path" }
    }
    foreach ($component in $lock.components) {
        if (-not (Test-Path (Join-Path $Root "licenses/$($component.name)-LICENSE.txt"))) {
            throw "CUDA license missing: $($component.name)"
        }
    }
}

if (Test-Path $Destination) {
    if (-not (Test-Path $marker) -or (Get-Content $marker -Raw).Trim() -ne $lockHash) {
        throw "Destination is not this pinned toolkit: $Destination. Choose an empty destination."
    }
    Assert-Toolkit $Destination
} else {
    $cache = "$Destination.archives"
    $staging = "$Destination.partial"
    if (Test-Path $staging) { throw "Incomplete staging directory exists: $staging" }
    New-Item -ItemType Directory -Path $cache, $staging -Force | Out-Null
    New-Item -ItemType Directory -Path (Join-Path $staging 'licenses') | Out-Null
    try {
        foreach ($component in $lock.components) {
            $archive = Join-Path $cache ([IO.Path]::GetFileName($component.archive))
            if (-not (Test-Path $archive)) {
                Write-Host "Downloading $($component.name) ($($lock.version))"
                Invoke-WebRequest -Uri ($lock.baseUrl + $component.archive) -OutFile "$archive.partial"
                Move-Item "$archive.partial" $archive -Force
            }
            if ((Get-FileHash $archive -Algorithm SHA256).Hash -ne $component.sha256) {
                throw "SHA256 mismatch: $archive. Delete this cached archive and retry."
            }
            $expanded = Join-Path $staging "extract-$($component.name)"
            Expand-Archive -LiteralPath $archive -DestinationPath $expanded
            $roots = @(Get-ChildItem $expanded -Directory)
            if ($roots.Count -ne 1) { throw "Unexpected archive layout: $archive" }
            $root = $roots[0].FullName
            foreach ($directory in @('bin', 'include', 'lib', 'nvvm')) {
                $source = Join-Path $root $directory
                if (Test-Path $source) {
                    $target = Join-Path $staging $directory
                    New-Item -ItemType Directory -Path $target -Force | Out-Null
                    Get-ChildItem $source | Copy-Item -Destination $target -Recurse -Force
                }
            }
            $license = Join-Path $root 'LICENSE'
            if (-not (Test-Path $license)) { throw "Archive has no LICENSE: $archive" }
            Copy-Item $license (Join-Path $staging "licenses/$($component.name)-LICENSE.txt")
            Get-ChildItem $root -File | Where-Object Name -Match '^(NOTICE|EULA)' | ForEach-Object {
                Copy-Item $_.FullName (Join-Path $staging "licenses/$($component.name)-$($_.Name).txt")
            }
            Remove-Item $expanded -Recurse -Force
        }
        Assert-Toolkit $staging
        Set-Content (Join-Path $staging 'transcriptor-cuda-lock.txt') $lockHash
        Move-Item $staging $Destination
    } catch {
        Remove-Item $staging -Recurse -Force -ErrorAction SilentlyContinue
        throw
    }
}

$env:CUDA_PATH = $Destination
$env:CUDAToolkit_ROOT = $Destination
$env:CUDA_LICENSE_DIR = Join-Path $Destination 'licenses'
$env:PATH = (Join-Path $Destination 'bin') + [IO.Path]::PathSeparator + $env:PATH
Write-Host "Pinned CUDA $($lock.version) ready in $Destination"
