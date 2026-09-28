param([string]$Exe = "$PSScriptRoot\..\..\dist\BrightnessTray.exe", [string]$Out = "$PSScriptRoot\..\images")
# Captures the real popup (demo data, 2x DPI) in dark and light themes and
# masks the rounded corners to transparency. Stops any running instance.
Add-Type -AssemblyName System.Drawing
Add-Type @'
using System; using System.Runtime.InteropServices;
public static class Cap {
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern IntPtr FindWindowW(string c, string n);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern bool PostMessageW(IntPtr h, uint m, IntPtr w, IntPtr l);
  [DllImport("user32.dll")] public static extern bool SetProcessDpiAwarenessContext(IntPtr v);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
}
'@
[Cap]::SetProcessDpiAwarenessContext([IntPtr]-4) | Out-Null
New-Item -ItemType Directory -Force $Out | Out-Null

function Mask-Corners([System.Drawing.Bitmap]$src, [int]$radius) {
  $w = $src.Width; $h = $src.Height
  $dst = New-Object System.Drawing.Bitmap $w, $h, ([System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
  $g = [System.Drawing.Graphics]::FromImage($dst); $g.DrawImage($src, 0, 0, $w, $h); $g.Dispose()
  foreach ($c in @(@(0,0), @(($w-$radius),0), @(0,($h-$radius)), @(($w-$radius),($h-$radius)))) {
    for ($y = $c[1]; $y -lt $c[1] + $radius; $y++) {
      for ($x = $c[0]; $x -lt $c[0] + $radius; $x++) {
        $cx = if ($x -lt $w/2) { $radius } else { $w - $radius }
        $cy = if ($y -lt $h/2) { $radius } else { $h - $radius }
        $dx = [Math]::Max(0, [Math]::Abs($x + 0.5 - $cx) - 0) ; $dy = [Math]::Abs($y + 0.5 - $cy)
        $inCorner = (($x -lt $radius -or $x -ge $w - $radius) -and ($y -lt $radius -or $y -ge $h - $radius))
        if (-not $inCorner) { continue }
        $d = [Math]::Sqrt($dx*$dx + $dy*$dy) - $radius
        $a = [Math]::Min(1, [Math]::Max(0, 0.5 - $d))
        $p = $dst.GetPixel($x, $y)
        $dst.SetPixel($x, $y, [System.Drawing.Color]::FromArgb([int](255 * $a), $p.R, $p.G, $p.B))
      }
    }
  }
  return $dst
}

foreach ($theme in 'dark', 'light') {
  Get-Process BrightnessTray -ErrorAction SilentlyContinue | Stop-Process -Force
  Start-Sleep -Milliseconds 400
  $env:BRIGHTNESS_TRAY_DEMO = '1'; $env:BRIGHTNESS_TRAY_DPI = '192'; $env:BRIGHTNESS_TRAY_THEME = $theme
  $p = Start-Process $Exe -ArgumentList '--background' -PassThru
  Remove-Item Env:\BRIGHTNESS_TRAY_DEMO, Env:\BRIGHTNESS_TRAY_DPI, Env:\BRIGHTNESS_TRAY_THEME
  Start-Sleep -Milliseconds 800
  $h = [Cap]::FindWindowW('BrightnessTray.Popup', [NullString]::Value)
  $ok = $false
  for ($try = 0; $try -lt 5 -and -not $ok; $try++) {
    [Cap]::PostMessageW($h, 0x8003, [IntPtr]0, [IntPtr]0) | Out-Null   # WM_SHOW
    Start-Sleep -Milliseconds 700                                       # let the demo rows arrive and paint
    if ([Cap]::IsWindowVisible($h)) {
      $r = New-Object Cap+RECT; [Cap]::GetWindowRect($h, [ref]$r) | Out-Null
      $bmp = New-Object System.Drawing.Bitmap ($r.R - $r.L), ($r.B - $r.T)
      $g = [System.Drawing.Graphics]::FromImage($bmp); $g.CopyFromScreen($r.L, $r.T, 0, 0, $bmp.Size); $g.Dispose()
      $ok = [Cap]::IsWindowVisible($h)   # still visible after the capture?
      # Drop the 1px DWM border: it is blended with whatever is behind the window.
      $bmp = $bmp.Clone((New-Object System.Drawing.Rectangle 1, 1, ($bmp.Width - 2), ($bmp.Height - 2)), $bmp.PixelFormat)
      if ($ok) { (Mask-Corners $bmp 16).Save("$Out\popup-$theme.png", [System.Drawing.Imaging.ImageFormat]::Png); "popup-$theme.png $($bmp.Width)x$($bmp.Height)" }
    }
  }
  if (-not $ok) { "FAILED to capture $theme" }
  Stop-Process -Id $p.Id -Force
}
