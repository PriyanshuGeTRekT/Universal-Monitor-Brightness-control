# Stops Brightness Tray, removes it from startup, deletes its settings and the installed exe.
Get-Process BrightnessTray -ErrorAction SilentlyContinue | Stop-Process -Force
Remove-ItemProperty 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' -Name BrightnessTray -ErrorAction SilentlyContinue
Remove-Item 'HKCU:\Software\BrightnessTray' -Recurse -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 300
Remove-Item "$env:LOCALAPPDATA\Programs\BrightnessTray\BrightnessTray.exe" -ErrorAction SilentlyContinue
"Brightness Tray removed."
