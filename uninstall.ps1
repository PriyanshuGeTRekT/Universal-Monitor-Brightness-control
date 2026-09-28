# Stops Brightness Tray, removes it from startup and deletes the installed exe.
Get-Process BrightnessTray -ErrorAction SilentlyContinue | Stop-Process -Force
Remove-ItemProperty 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' -Name BrightnessTray -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 300
Remove-Item "$env:LOCALAPPDATA\Programs\BrightnessTray\BrightnessTray.exe" -ErrorAction SilentlyContinue
"Brightness Tray removed."
