# Checks that an ffmpeg.exe has everything the app uses, and runs the record -> save pipeline
# from src-tauri/src/recorder.rs on a test pattern. Exits non-zero on the first problem.
#
#   scripts/test-ffmpeg.ps1 [path to ffmpeg.exe]
#
# Screen capture itself (gfxcapture, ddagrab, gdigrab) can't run on a CI machine, so it only checks they exist.
param([string]$Ffmpeg = "$PSScriptRoot/../src-tauri/binaries/ffmpeg-x86_64-pc-windows-msvc.exe")
$ErrorActionPreference = 'Stop'
$Ffmpeg = (Resolve-Path $Ffmpeg).Path # absolute: the pipeline below runs in a temp folder

function Fail([string]$msg) { Write-Host "FAIL: $msg"; exit 1 }

# 1. Every component recorder.rs names. "ffmpeg -h <kind>=<name>" prints "Unknown ..." when it's missing.
$needed = @{
    encoder = 'h264_nvenc', 'h264_amf', 'h264_qsv', 'libx264', 'aac'
    decoder = 'wrapped_avframe', 'bmp', 'h264', 'pcm_s16le' # lavfi and gdigrab frames; reading segments back; sound
    filter  = 'gfxcapture', 'ddagrab', 'hwdownload', 'format', 'scale', 'color', 'anull', 'aformat', 'aresample'
    demuxer = 'gdigrab', 'lavfi', 'mpegts', 'mov', 's16le' # mov: only for this test's clip check
    muxer   = 'segment', 'mpegts', 'mp4', 'null'
}
foreach ($kind in $needed.Keys) {
    foreach ($name in $needed[$kind]) {
        $help = & $Ffmpeg -hide_banner -h "$kind=$name" 2>&1 | Out-String
        if ($help -cmatch "(?m)^Unknown $kind") { Fail "missing $kind '$name'" }
    }
}
$protocols = & $Ffmpeg -hide_banner -protocols 2>&1 | Out-String
if ($protocols -notmatch '(?m)^\s*concat\s*$') { Fail "missing protocol 'concat'" }
Write-Host "Components: OK"

# 2. Record 4 seconds into rolling 1-second segments, the way Recorder::start does (libx264 path).
$dir = Join-Path ([IO.Path]::GetTempPath()) "lol_replay_ffmpeg_test"
Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue
New-Item -ItemType Directory $dir | Out-Null
Push-Location $dir
try {
    & $Ffmpeg -hide_banner -loglevel error -y -f lavfi -i 'color=c=gold:s=640x360:r=30:d=4' `
        -vf "scale=-2:'min(360,ih)':flags=fast_bilinear,format=yuv420p" -c:v libx264 `
        -preset ultrafast -crf 23 -tune zerolatency -g 30 -an -f segment `
        -segment_time 1 -segment_wrap 7 -segment_format mpegts 'seg%03d.ts'
    if ($LASTEXITCODE -ne 0) { Fail "recording segments failed" }
    $segs = Get-ChildItem -Filter *.ts | Sort-Object Name
    if ($segs.Count -lt 3) { Fail "expected at least 3 segments, got $($segs.Count)" }
    Write-Host "Recording: OK ($($segs.Count) segments)"

    # 3. Save the last 2 seconds, the way Recorder::save_last does.
    $concat = 'concat:' + (($segs | ForEach-Object Name) -join '|')
    & $Ffmpeg -hide_banner -loglevel error -y -sseof -2 -i $concat -c copy -movflags +faststart clip.mp4
    if ($LASTEXITCODE -ne 0 -or -not (Test-Path clip.mp4)) { Fail "saving the clip failed" }

    # 4. Check the saved clip is a real video of about the requested length. Copying cuts on
    # keyframes (1 s apart here), so "the last 2 seconds" can come out anywhere from 2 to 3.
    # "-i" with no output always exits non-zero, so only its text matters. Its text is on stderr,
    # which Windows PowerShell 5.1 turns into a terminating error under 'Stop'.
    $ErrorActionPreference = 'Continue'
    $info = & $Ffmpeg -hide_banner -i clip.mp4 2>&1 | Out-String
    $ErrorActionPreference = 'Stop'
    if ($info -notmatch 'Stream #\d+:\d+.*Video: h264') { Fail "clip has no h264 video stream:`n$info" }
    if ($info -notmatch 'Duration: (\d+):(\d+):([\d.]+)') { Fail "clip has no duration:`n$info" }
    $secs = [int]$Matches[1] * 3600 + [int]$Matches[2] * 60 + [double]::Parse($Matches[3], [cultureinfo]::InvariantCulture)
    if ($secs -lt 1.5 -or $secs -gt 3.5) { Fail "clip is $secs s long, expected about 2 (did -sseof work?)" }
    Write-Host "Clip: OK ($secs s)"

    # 5. Save it again with sound, the way save_stream does: 3 s of raw 48 kHz stereo PCM (a tone)
    # beside the video, encoded to AAC.
    $samples = New-Object int16[] (48000 * 2 * 3)
    for ($i = 0; $i -lt $samples.Length; $i++) { $samples[$i] = [int16](3000 * [math]::Sin([math]::Floor($i / 2) * 440 * 2 * [math]::PI / 48000)) }
    $bytes = New-Object byte[] ($samples.Length * 2)
    [Buffer]::BlockCopy($samples, 0, $bytes, 0, $bytes.Length)
    [IO.File]::WriteAllBytes((Join-Path $dir 'sound.pcm'), $bytes)
    & $Ffmpeg -hide_banner -loglevel error -y -sseof -2 -i $concat -f s16le -ar 48000 -ac 2 -i sound.pcm `
        -map 0:v -map 1:a -c:a aac -b:a 128k -c:v copy -movflags +faststart sound.mp4
    if ($LASTEXITCODE -ne 0 -or -not (Test-Path sound.mp4)) { Fail "saving the clip with sound failed" }
    $ErrorActionPreference = 'Continue'
    $info = & $Ffmpeg -hide_banner -i sound.mp4 2>&1 | Out-String
    $ErrorActionPreference = 'Stop'
    if ($info -notmatch 'Stream #\d+:\d+.*Video: h264') { Fail "clip with sound has no h264 video stream:`n$info" }
    if ($info -notmatch 'Stream #\d+:\d+.*Audio: aac.*48000 Hz, stereo') { Fail "clip with sound has no AAC sound:`n$info" }
    Write-Host "Clip with sound: OK"

    Write-Host "Saving: OK"
} finally {
    Pop-Location
    Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue
}
Write-Host "ffmpeg is good to ship."
exit 0 # not ffmpeg's: the "-i" check above leaves a non-zero $LASTEXITCODE behind
