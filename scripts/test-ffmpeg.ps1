# Checks that an ffmpeg.exe has everything the app uses, and runs the record -> cut -> save pipeline
# from src-tauri/src/recorder.rs on a test pattern. Exits non-zero on the first problem.
#
#   scripts/test-ffmpeg.ps1 [path to ffmpeg.exe]
#
# Screen capture itself (gfxcapture, ddagrab, gdigrab) can't run on a CI machine, so it only checks they exist.
# The pipeline runs with libx264, and again with each AV1 encoder that works on this PC (none on CI).
param([string]$Ffmpeg = "$PSScriptRoot/../src-tauri/binaries/ffmpeg-x86_64-pc-windows-msvc.exe")
$ErrorActionPreference = 'Stop'
$Ffmpeg = (Resolve-Path $Ffmpeg).Path # absolute: the pipeline below runs in a temp folder

function Fail([string]$msg) { Write-Host "FAIL: $msg"; exit 1 }

# 1. Every component recorder.rs names. "ffmpeg -h <kind>=<name>" starts with "<Kind> <name>" when it's
# there. Missing ones each say so differently ("Unknown format", "Codec ... is not recognized", ...).
$needed = @{
    encoder = 'av1_nvenc', 'av1_amf', 'av1_qsv', 'h264_nvenc', 'h264_amf', 'h264_qsv', 'libx264', 'aac'
    decoder = 'wrapped_avframe', 'bmp', 'h264', 'pcm_s16le' # lavfi and gdigrab frames; reading clips back; sound
    filter  = 'gfxcapture', 'ddagrab', 'hwdownload', 'format', 'scale', 'color', 'anull', 'aformat', 'aresample'
    demuxer = 'gdigrab', 'lavfi', 'matroska', 'mov', 's16le' # mov: only for this test's clip check
    muxer   = 'matroska', 'mp4', 'null'
}
foreach ($kind in $needed.Keys) {
    foreach ($name in $needed[$kind]) {
        $help = & $Ffmpeg -hide_banner -h "$kind=$name" 2>&1 | Out-String
        if ($help -notmatch "(?m)^$kind $([regex]::Escape($name))\b") { Fail "missing $kind '$name'" }
    }
}
# Parsers (h264, av1) can't be listed; the clips below only read back right with them.
$protocols = & $Ffmpeg -hide_banner -protocols 2>&1 | Out-String
if ($protocols -notmatch '(?m)^\s*pipe\s*$') { Fail "missing protocol 'pipe'" }
Write-Host "Components: OK"

# A Matroska element's header at $at: its id, where its body starts, and its size (-1: unknown).
function Read-Element([byte[]]$b, [long]$at) {
    $vint = {
        param([long]$p, [bool]$keepMarker)
        $len = 1
        while ($len -le 8 -and -not ($b[$p] -band (0x80 -shr ($len - 1)))) { $len++ }
        $v = [long]$b[$p]
        if (-not $keepMarker) { $v = $v -band ((0xFF -shr $len)) }
        for ($i = 1; $i -lt $len; $i++) { $v = ($v -shl 8) -bor $b[$p + $i] }
        $allOnes = ([long]1 -shl (7 * $len)) - 1
        , @($v, $len, ($v -eq $allOnes))
    }
    $id = & $vint $at $true
    $size = & $vint ($at + $id[1]) $false
    @{ Id = $id[0]; Body = $at + $id[1] + $size[1]; Size = $(if ($size[2]) { -1 } else { $size[0] }) }
}

# What Ring::since_keyframe_before does: the header (everything before the first cluster), then
# the clusters from the last keyframe at least $back clusters (frames) before the end.
function Cut-Recording([byte[]]$b, [int]$back) {
    $ebml = Read-Element $b 0
    $segment = Read-Element $b ($ebml.Body + $ebml.Size)
    if ($segment.Id -ne 0x18538067) { Fail "the recording has no Segment" }
    $at = $segment.Body
    $headerEnd = -1
    $clusters = @()
    while ($at -lt $b.Length) {
        $e = Read-Element $b $at
        if ($e.Size -lt 0) { Fail "element $($e.Id) of unknown size" }
        if ($e.Id -eq 0x1F43B675) {
            if ($headerEnd -lt 0) { $headerEnd = $at }
            # Its children: the Timestamp, then the frame's SimpleBlock (track, 2-byte time, flags).
            $key = $false
            $c = $e.Body
            while ($c -lt $e.Body + $e.Size) {
                $k = Read-Element $b $c
                if ($k.Id -eq 0xA3) { $key = [bool]($b[$k.Body + 3] -band 0x80); break }
                $c = $k.Body + $k.Size
            }
            $clusters += , @($at, ($e.Body + $e.Size), $key)
        }
        $at = $e.Body + $e.Size
    }
    if ($clusters.Count -lt $back) { Fail "only $($clusters.Count) frames recorded" }
    $start = $clusters.Count - $back
    while ($start -gt 0 -and -not $clusters[$start][2]) { $start-- }
    if (-not $clusters[$start][2]) { Fail "no keyframe in the recording" }
    $last = $clusters[$clusters.Count - 1][1]
    $out = New-Object byte[] ($headerEnd + $last - $clusters[$start][0])
    [Array]::Copy($b, 0, $out, 0, $headerEnd)
    [Array]::Copy($b, $clusters[$start][0], $out, $headerEnd, $last - $clusters[$start][0])
    $out
}

# Saving reads stdin and recording writes stdout, as raw bytes: cmd's redirection keeps them
# intact, where Windows PowerShell 5.1's would turn them into text.
function Invoke-Piped([string]$cmdline) {
    cmd /c "`"$Ffmpeg`" -hide_banner -loglevel error -y $cmdline"
    $LASTEXITCODE -eq 0
}

function Get-Info([string]$file) {
    # "-i" with no output always exits non-zero, so only its text matters. Its text is on stderr,
    # which Windows PowerShell 5.1 turns into a terminating error under 'Stop'.
    $ErrorActionPreference = 'Continue'
    $info = & $Ffmpeg -hide_banner -i $file 2>&1 | Out-String
    $ErrorActionPreference = 'Stop'
    $info
}

# 2-5. Record 4 seconds the way Recorder::start does, cut the last 2 the way Ring does, and save
# them with and without sound the way save_stream does.
function Test-Pipeline([string]$encoder, [string]$encoderArgs, [string]$pix, [string]$codec) {
    # Like encoder_works: whether the encoder runs here at all, with these arguments.
    if (-not (Invoke-Piped "-f lavfi -i color=s=256x256:d=0.2 -vf format=nv12 -c:v $encoder $encoderArgs -f matroska NUL 2>NUL")) { return $false }

    $ok = Invoke-Piped ("-f lavfi -i color=c=gold:s=640x360:r=30:d=4 " +
        "-vf `"scale=-2:'min(360,ih)':flags=fast_bilinear,format=$pix`" -c:v $encoder $encoderArgs -g 30 " +
        "-an -flush_packets 1 -f matroska -cluster_size_limit 0 -write_crc32 0 pipe:1 > rec.mkv")
    if (-not $ok -or -not (Test-Path rec.mkv)) { Fail "$encoder`: recording failed" }
    $cut = Cut-Recording ([IO.File]::ReadAllBytes((Join-Path $dir 'rec.mkv'))) 60
    [IO.File]::WriteAllBytes((Join-Path $dir 'cut.mkv'), $cut)
    Write-Host "$encoder recording: OK"

    $ok = Invoke-Piped "-f matroska -i pipe:0 -c:v copy -movflags +faststart clip.mp4 < cut.mkv"
    if (-not $ok -or -not (Test-Path clip.mp4)) { Fail "$encoder`: saving the clip failed" }
    # Cut on a keyframe 1 s apart, so "the last 2 seconds" can come out anywhere from 2 to 3.
    $info = Get-Info clip.mp4
    if ($info -notmatch "Stream #\d+:\d+.*Video: $codec") { Fail "$encoder`: clip has no $codec video stream:`n$info" }
    if ($info -notmatch 'Duration: (\d+):(\d+):([\d.]+)') { Fail "$encoder`: clip has no duration:`n$info" }
    $secs = [int]$Matches[1] * 3600 + [int]$Matches[2] * 60 + [double]::Parse($Matches[3], [cultureinfo]::InvariantCulture)
    if ($secs -lt 1.5 -or $secs -gt 3.5) { Fail "$encoder`: clip is $secs s long, expected about 2" }
    $bytes = [IO.File]::ReadAllBytes((Join-Path $dir 'clip.mp4'))
    $text = [Text.Encoding]::ASCII.GetString($bytes)
    if ($text.IndexOf('moov') -gt $text.IndexOf('mdat')) { Fail "$encoder`: the clip's index isn't at the start (+faststart)" }
    Write-Host "$encoder clip: OK ($secs s)"

    # With sound: 3 s of raw 48 kHz stereo PCM (a tone) beside the video, encoded to AAC.
    $ok = Invoke-Piped ("-f matroska -i pipe:0 -f s16le -ar 48000 -ac 2 -i sound.pcm " +
        "-map 0:v -map 1:a -c:a aac -b:a 128k -c:v copy -movflags +faststart sound.mp4 < cut.mkv")
    if (-not $ok -or -not (Test-Path sound.mp4)) { Fail "$encoder`: saving the clip with sound failed" }
    $info = Get-Info sound.mp4
    if ($info -notmatch "Stream #\d+:\d+.*Video: $codec") { Fail "$encoder`: clip with sound has no $codec video stream:`n$info" }
    if ($info -notmatch 'Stream #\d+:\d+.*Audio: aac.*48000 Hz, stereo') { Fail "$encoder`: clip with sound has no AAC sound:`n$info" }
    Write-Host "$encoder clip with sound: OK"
    Remove-Item rec.mkv, cut.mkv, clip.mp4, sound.mp4
    $true
}

$dir = Join-Path ([IO.Path]::GetTempPath()) "lol_replay_ffmpeg_test"
Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue
New-Item -ItemType Directory $dir | Out-Null
Push-Location $dir
try {
    $samples = New-Object int16[] (48000 * 2 * 3)
    for ($i = 0; $i -lt $samples.Length; $i++) { $samples[$i] = [int16](3000 * [math]::Sin([math]::Floor($i / 2) * 440 * 2 * [math]::PI / 48000)) }
    $bytes = New-Object byte[] ($samples.Length * 2)
    [Buffer]::BlockCopy($samples, 0, $bytes, 0, $bytes.Length)
    [IO.File]::WriteAllBytes((Join-Path $dir 'sound.pcm'), $bytes)

    # The arguments are encoder_args in recorder.rs.
    if (-not (Test-Pipeline 'libx264' '-preset ultrafast -crf 27 -tune zerolatency' 'yuv420p' 'h264')) { Fail "libx264 doesn't run" }
    $av1 = @{
        av1_nvenc = '-preset p4 -rc vbr -cq 34 -delay 0'
        av1_amf   = '-quality speed -rc cqp -qp_i 136 -qp_p 136'
        av1_qsv   = '-preset veryfast -global_quality 34'
    }
    foreach ($encoder in $av1.Keys | Sort-Object) {
        if (-not (Test-Pipeline $encoder $av1[$encoder] 'nv12' 'av1')) { Write-Host "$encoder`: not on this PC, skipped" }
    }
    Write-Host "Saving: OK"
} finally {
    Pop-Location
    Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue
}
Write-Host "ffmpeg is good to ship."
exit 0 # not ffmpeg's: the "-i" checks above leave a non-zero $LASTEXITCODE behind
