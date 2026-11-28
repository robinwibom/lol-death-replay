# Copies the newest saved clip that has recorded keys and clicks into this folder, as the clip
# the test page plays. Clips aren't committed (see .gitignore).
#   powershell -File prototypes/player-harness/get-clip.ps1 [path\to\clip.mp4]
param([string]$Clip)

if (-not $Clip) {
    $root = Join-Path $env:USERPROFILE 'Videos\LoLDeathReplay'
    $sidecar = Get-ChildItem $root -Recurse -Filter *.inputs.json -ErrorAction SilentlyContinue |
        Sort-Object LastWriteTime -Descending | Select-Object -First 1
    if (-not $sidecar) { Write-Error "No clip with recorded inputs under $root"; exit 1 }
    $Clip = $sidecar.FullName -replace '\.inputs\.json$', '.mp4'
}
$inputs = $Clip -replace '\.mp4$', '.inputs.json'
foreach ($f in $Clip, $inputs) { if (-not (Test-Path $f)) { Write-Error "Missing $f"; exit 1 } }

Copy-Item $Clip (Join-Path $PSScriptRoot 'clip.mp4') -Force
Copy-Item $inputs (Join-Path $PSScriptRoot 'clip.inputs.json') -Force
Write-Output "Copied $Clip"
