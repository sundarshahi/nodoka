# Installs nodoka on Windows: downloads the release for this machine, checks its SHA-256, and sets it up.
#   irm https://raw.githubusercontent.com/sundarshahi/nodoka/main/install.ps1 | iex
# Pick a version with $env:NODOKA_VERSION = 'v0.3.0'.
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue' # the progress bar makes downloads slow in Windows PowerShell

$repo = if ($env:NODOKA_REPO) { $env:NODOKA_REPO } else { 'sundarshahi/nodoka' }
$version = if ($env:NODOKA_VERSION) { $env:NODOKA_VERSION } else { 'latest' }
$arch = if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') { 'aarch64' } else { 'x86_64' }
$file = "nodoka-$arch-pc-windows-msvc.zip"
$base = if ($version -eq 'latest') { "https://github.com/$repo/releases/latest/download" } else { "https://github.com/$repo/releases/download/$version" }
if ($env:NODOKA_BASE_URL) { $base = $env:NODOKA_BASE_URL } # a mirror, or a local folder for testing

$tmp = Join-Path ([IO.Path]::GetTempPath()) ("nodoka-" + [guid]::NewGuid())
New-Item -ItemType Directory $tmp | Out-Null
try {
    Write-Host "Downloading $file..."
    foreach ($f in @($file, "$file.sha256")) {
        if ($base -match '^https?://') { Invoke-WebRequest "$base/$f" -OutFile (Join-Path $tmp $f) -UseBasicParsing }
        else { Copy-Item (Join-Path $base $f) (Join-Path $tmp $f) }
    }
    $want = ((Get-Content (Join-Path $tmp "$file.sha256") -Raw).Trim() -split '\s+')[0].ToLower()
    $got = (Get-FileHash (Join-Path $tmp $file) -Algorithm SHA256).Hash.ToLower()
    if ($want -ne $got) { throw "The download's checksum doesn't match. Nothing was installed." }
    Expand-Archive (Join-Path $tmp $file) -DestinationPath $tmp
    & (Join-Path $tmp 'nodoka.exe') install
    if ($LASTEXITCODE -ne 0) { throw 'nodoka install failed (see above).' }
}
finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
