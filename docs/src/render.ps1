# Renders the README images (docs/src/*.html -> docs/images/*.png) at 2x with
# headless Microsoft Edge. Run capture.ps1 first to refresh the popup shots.
Add-Type -AssemblyName System.Drawing
$edge = "${env:ProgramFiles(x86)}\Microsoft\Edge\Application\msedge.exe"
$src = $PSScriptRoot
$out = Resolve-Path "$PSScriptRoot\..\images"
$profileDir = "$env:TEMP\brightness-tray-edge"
# name, CSS width x height, published file name
$pages = @(@('banner', 1280, 600, 'banner'), @('where', 1280, 720, 'tray'), @('architecture', 1280, 720, 'architecture'))
foreach ($p in $pages) {
  $name, $w, $h, $file = $p
  $png = "$out\$file.png"
  $url = 'file:///' + ("$src\$name.html" -replace '\\', '/')
  # Render 180px taller than needed and crop: headless Edge sometimes leaves
  # a tile near the bottom edge unpainted.
  $args = @('--headless=new', '--disable-gpu', '--hide-scrollbars', '--force-device-scale-factor=2',
            "--user-data-dir=$profileDir", "--window-size=$w,$($h + 180)", "--screenshot=$png", '--virtual-time-budget=1500', $url)
  Start-Process -FilePath $edge -ArgumentList $args -Wait -WindowStyle Hidden
  $img = [System.Drawing.Bitmap]::FromFile($png)
  $crop = $img.Clone((New-Object System.Drawing.Rectangle 0, 0, ($w * 2), ($h * 2)), $img.PixelFormat)
  $img.Dispose()
  $crop.Save($png, [System.Drawing.Imaging.ImageFormat]::Png)
  $crop.Dispose()
  "$file.png"
}
