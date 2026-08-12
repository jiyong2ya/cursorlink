Add-Type -AssemblyName System.Drawing

$InputPng = "C:\dev-jiyong\cursorlink\assets\icon.png"
$OutputIco = "C:\dev-jiyong\cursorlink\assets\cursorlink.ico"
$Sizes = @(256, 128, 64, 48, 32, 16)

$src = [System.Drawing.Bitmap]::FromFile($InputPng)

$imageDatas = New-Object System.Collections.ArrayList
foreach ($size in $Sizes) {
    $resized = New-Object System.Drawing.Bitmap($size, $size)
    $g = [System.Drawing.Graphics]::FromImage($resized)
    $g.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
    $g.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
    $g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::HighQuality
    $g.CompositingQuality = [System.Drawing.Drawing2D.CompositingQuality]::HighQuality
    $g.DrawImage($src, 0, 0, $size, $size)
    $g.Dispose()

    $stream = New-Object System.IO.MemoryStream
    $resized.Save($stream, [System.Drawing.Imaging.ImageFormat]::Png)
    [void]$imageDatas.Add($stream.ToArray())
    $resized.Dispose()
    $stream.Dispose()
}
$src.Dispose()

$ms = New-Object System.IO.MemoryStream
$bw = New-Object System.IO.BinaryWriter($ms)

# ICONDIR
$bw.Write([UInt16]0)                # reserved
$bw.Write([UInt16]1)                # type = ICO
$bw.Write([UInt16]$Sizes.Count)     # count

# ICONDIRENTRYs
$offset = 6 + ($Sizes.Count * 16)
for ($i = 0; $i -lt $Sizes.Count; $i++) {
    $size = $Sizes[$i]
    $dataLen = $imageDatas[$i].Length
    $sizeByte = if ($size -ge 256) { 0 } else { $size }
    $bw.Write([Byte]$sizeByte)      # width
    $bw.Write([Byte]$sizeByte)      # height
    $bw.Write([Byte]0)              # colors
    $bw.Write([Byte]0)              # reserved
    $bw.Write([UInt16]1)            # planes
    $bw.Write([UInt16]32)           # bpp
    $bw.Write([UInt32]$dataLen)     # data size
    $bw.Write([UInt32]$offset)      # offset
    $offset += $dataLen
}

# Image data
foreach ($data in $imageDatas) {
    $bw.Write($data)
}

[System.IO.File]::WriteAllBytes($OutputIco, $ms.ToArray())
$bw.Dispose()
$ms.Dispose()

Write-Host "Created: $OutputIco ($($Sizes.Count) sizes: $($Sizes -join ', '))"
