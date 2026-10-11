# Resolve the Windows SDK through its registered install root (it need not be on C:).
if (!$env:GPUI_FXC_PATH) {
    $sdkRoot = (Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows Kits\Installed Roots' -ErrorAction SilentlyContinue).KitsRoot10
    $roots = @($sdkRoot, 'C:\Program Files (x86)\Windows Kits\10') | Where-Object { $_ -and (Test-Path $_) } | Select-Object -Unique
    foreach ($sdk in $roots) {
        $fxc = Get-ChildItem (Join-Path $sdk 'bin') -Recurse -Filter fxc.exe -ErrorAction SilentlyContinue | Where-Object { $_.Directory.Name -eq 'x64' } | Sort-Object FullName -Descending | Select-Object -First 1
        if ($fxc) { $env:GPUI_FXC_PATH = $fxc.FullName; break }
    }
}
