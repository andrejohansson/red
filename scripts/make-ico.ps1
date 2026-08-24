# Regenerate assets/red.ico from the brand SVG (assets/red.svg).
#
#   pwsh scripts/make-ico.ps1 [-Source assets/red.svg] [-Out assets/red.ico]
#
# Unlike the macOS .icns (built at bundle time by scripts/make-icns.sh) and the
# Linux PNGs (rendered by scripts/bundle-linux.sh), this output is **committed**:
# crates/red/build.rs embeds it into RED.exe on every build, so requiring an SVG
# rasteriser on every developer's machine to compile RED would be the wrong trade.
# Run this only when the brand mark changes, and commit the result.
#
# Needs: ImageMagick 7 (`magick`), which must have an SVG delegate. Verify the
# corners came out rounded rather than square -- see the check at the end.
#
# Each size is rendered from the *vector* rather than downsampled from one large
# raster: at 16x16 the corner radius is about 3px, and resampling a 1028px master
# muddies it.
param(
  [string]$Source = "assets/red.svg",
  [string]$Out    = "assets/red.ico"
)
$ErrorActionPreference = "Stop"
$Root = Resolve-Path (Join-Path $PSScriptRoot "..")
$Src  = Join-Path $Root $Source
$Dest = Join-Path $Root $Out
if (-not (Test-Path $Src)) { throw "missing $Src" }
if (-not (Get-Command magick -ErrorAction SilentlyContinue)) {
  throw "ImageMagick 7 (magick) not found on PATH"
}

# The sizes Windows actually asks for: 16 (title bar, small taskbar), 24, 32
# (Alt-Tab), 48 (Explorer medium), 64, 128, 256 (Explorer extra-large / jumbo).
# 256 is stored PNG-compressed inside the .ico, which is why the file is ~100 KB
# rather than the ~350 KB an all-BMP icon would be.
$Sizes = 16, 24, 32, 48, 64, 128, 256
$Tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("red-ico-" + [System.Guid]::NewGuid())
New-Item -ItemType Directory -Force -Path $Tmp | Out-Null
try {
  $frames = foreach ($s in $Sizes) {
    $png = Join-Path $Tmp "$s.png"
    # -density high enough that the rasteriser works from a fine grid before
    # resizing to the target, so small sizes keep clean antialiased corners.
    magick -background none -density 1200 $Src -resize "${s}x${s}" "png32:$png"
    $png
  }
  magick @frames $Dest
  Write-Host "wrote $Dest"
  magick identify $Dest

  # A square corner means the SVG's `rx` was ignored (no SVG delegate, or a
  # renderer that dropped it), which is wrong in a way that is easy to miss at
  # icon sizes. Checked on the largest render, not the .ico: at 16x16 the corner
  # pixel is legitimately *antialiased* (the radius there is about 3px), so a
  # nonzero alpha would say nothing either way. At 256 the corner is ~50px outside
  # the curve and must be fully transparent.
  $largest = $frames | Select-Object -Last 1
  $corner = (magick $largest -format "%[fx:floor(u.a*255)]" info:)
  if ([int]$corner -ne 0) {
    Write-Warning "corner alpha is $corner, expected 0: the rounded corners did not render"
  } else {
    Write-Host "corners are rounded (256px corner is fully transparent)"
  }
} finally {
  Remove-Item -Recurse -Force $Tmp -ErrorAction SilentlyContinue
}
