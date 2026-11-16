$ErrorActionPreference = "Stop"
$bundle = "src-tauri/resources/windows-signed-runtime.tar.gz"
$cached = Test-Path $bundle
if ($cached) {
    New-Item -ItemType Directory -Force signed-runtime-work/runtime | Out-Null
    tar -xzf $bundle -C signed-runtime-work/runtime
    if ($LASTEXITCODE -ne 0) { throw "Cached signed runtime extraction failed" }
} else {
    python scripts/build-windows-signed-runtime.py prepare signed-runtime-work
    if ($LASTEXITCODE -ne 0) { throw "Windows runtime preparation failed" }
}

$peFiles = python scripts/build-windows-signed-runtime.py list-pe signed-runtime-work/runtime | ConvertFrom-Json
if ($LASTEXITCODE -ne 0 -or $peFiles.Count -eq 0) { throw "Windows runtime PE scan failed" }
Write-Host "Checking $($peFiles.Count) Windows executable files"
$unsigned = @($peFiles | Where-Object { (Get-AuthenticodeSignature -FilePath $_).Status -ne "Valid" })
if ($cached -and $unsigned.Count -ne 0) { throw "Cached runtime contains $($unsigned.Count) unsigned or invalid PE files" }
for ($i = 0; $i -lt $unsigned.Count; $i += 99) {
    $end = [Math]::Min($i + 98, $unsigned.Count - 1)
    $batch = @($unsigned[$i..$end])
    & artifact-signing-cli -e https://neu.codesigning.azure.net -a extra-headroom -c extraheadroom -d Headroom @batch
    if ($LASTEXITCODE -ne 0) { throw "Signing failed for PE files $i through $end" }
}
foreach ($file in $peFiles) {
    $signature = Get-AuthenticodeSignature -FilePath $file
    if ($signature.Status -ne "Valid") {
        throw "Invalid Authenticode signature: $file ($($signature.Status))"
    }
}

python scripts/build-windows-signed-runtime.py smoke signed-runtime-work/runtime
if ($LASTEXITCODE -ne 0) { throw "Relocated signed runtime did not start" }

if (-not $cached) {
    python scripts/build-windows-signed-runtime.py archive signed-runtime-work/runtime $bundle
    if ($LASTEXITCODE -ne 0) { throw "Signed runtime archive failed" }
}
$sha = (Get-FileHash -Path $bundle -Algorithm SHA256).Hash.ToLowerInvariant()
Add-Content -Path $env:GITHUB_ENV -Value "HEADROOM_SIGNED_RUNTIME_SHA256=$sha"
Write-Host "Signed Windows runtime SHA-256: $sha"
Add-Content -Path $env:GITHUB_STEP_SUMMARY -Value "Signed Windows runtime: $($peFiles.Count) PE files, archive SHA-256 $sha"
Remove-Item -Recurse -Force signed-runtime-work
