# Builds a release exe into .\dist. Works with the rustup GNU toolchain
# (no Visual Studio needed): its bundled dlltool just has to be on PATH.
$sysroot = & "$env:USERPROFILE\.cargo\bin\rustc.exe" --print sysroot
$env:PATH = "$env:USERPROFILE\.cargo\bin;$sysroot\lib\rustlib\x86_64-pc-windows-gnu\bin\self-contained;$env:PATH"
if (-not $env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR = "$env:TEMP\brightness-tray-target" }
cargo build --release --manifest-path "$PSScriptRoot\Cargo.toml"
if ($LASTEXITCODE) { exit $LASTEXITCODE }
New-Item -ItemType Directory -Force "$PSScriptRoot\dist" | Out-Null
Copy-Item "$env:CARGO_TARGET_DIR\release\BrightnessTray.exe" "$PSScriptRoot\dist\BrightnessTray.exe" -Force
Get-Item "$PSScriptRoot\dist\BrightnessTray.exe" | Select-Object Name, Length
