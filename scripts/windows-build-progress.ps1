# Read-only diagnostics: no command lines, environment values, or credentials.
$ErrorActionPreference = 'Stop'
$os = Get-CimInstance Win32_OperatingSystem
Write-Host ("Memory: {0:N0} MB free / {1:N0} MB total" -f ($os.FreePhysicalMemory / 1024), ($os.TotalVisibleMemorySize / 1024))
$names = @('rustc', 'cargo', 'nvcc', 'cicc', 'ptxas', 'cl', 'link', 'lld-link', 'ninja', 'cmake', 'lib')
Get-Process -Name $names -ErrorAction SilentlyContinue | Sort-Object CPU -Descending | Select-Object -First 12 | ForEach-Object {
    Write-Host ("  {0} pid={1} cpuSeconds={2:N1} memoryMB={3:N0}" -f $_.ProcessName, $_.Id, $_.CPU, ($_.WorkingSet64 / 1MB))
}
