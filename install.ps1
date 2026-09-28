# Installs dist\BrightnessTray.exe for the current user, registers it to
# start at logon, and launches it. Run build.ps1 first.
$dir = "$env:LOCALAPPDATA\Programs\BrightnessTray"
$exe = "$dir\BrightnessTray.exe"

Get-Process BrightnessTray -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 300
New-Item -ItemType Directory -Force $dir | Out-Null
Copy-Item "$PSScriptRoot\dist\BrightnessTray.exe" $exe -Force

# Same entry the tray menu's "Start with Windows" writes.
Set-ItemProperty 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' -Name BrightnessTray -Value "`"$exe`" --background"

Start-Process $exe
"Installed to $exe (starts with Windows)"
