# Builds dist/screen-translate-<version>.zip: the app plus the official llama.cpp Vulkan engine.
# The model is not in the package: users download it from the settings window.
$ErrorActionPreference = 'Stop'
$tag = 'b12144'
$asset = "llama-$tag-bin-win-vulkan-x64.zip"
$sha256 = '4824892a8bf28726a2790c8c9041e146c4357c51f6fb22184f8ac58985ed6384'

$root = Split-Path $PSScriptRoot
$version = (Select-String -Path "$root\Cargo.toml" -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value
$name = "screen-translate-$version"
$dist = "$root\dist"
$stage = "$dist\$name"

cargo build --release --manifest-path "$root\Cargo.toml"
if ($LASTEXITCODE -ne 0) { throw 'build failed' }

Remove-Item $stage -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force "$stage\engine", "$stage\data" | Out-Null
Set-Content "$stage\data\README.txt" -Encoding utf8 -Value 'Settings and models live here. Delete the whole app folder to remove the app.'
$zip = "$dist\$asset"
if (-not (Test-Path $zip)) {
    Invoke-WebRequest "https://github.com/ggml-org/llama.cpp/releases/download/$tag/$asset" -OutFile $zip
}
if ((Get-FileHash $zip -Algorithm SHA256).Hash.ToLower() -ne $sha256) {
    Remove-Item $zip
    throw "$asset does not match its expected hash"
}
Expand-Archive $zip "$stage\engine"

Copy-Item "$root\target\release\screen-translate.exe", "$root\README.md", "$root\LICENSE", "$root\NOTICE.md" $stage
Compress-Archive "$stage\*" "$dist\$name.zip" -Force
Write-Host "dist\$name.zip  $([int]((Get-Item "$dist\$name.zip").Length / 1MB)) MB"
