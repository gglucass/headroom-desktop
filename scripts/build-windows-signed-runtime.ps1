$ErrorActionPreference = "Stop"

python scripts/build-windows-signed-runtime.py prepare signed-runtime-work
if ($LASTEXITCODE -ne 0) { throw "Windows runtime preparation failed" }

$peFiles = python scripts/build-windows-signed-runtime.py list-pe signed-runtime-work/runtime | ConvertFrom-Json
if ($LASTEXITCODE -ne 0 -or $peFiles.Count -eq 0) { throw "Windows runtime PE scan failed" }
Write-Host "Checking $($peFiles.Count) Windows executable files"
foreach ($file in $peFiles) {
    $signature = Get-AuthenticodeSignature -FilePath $file
    if ($signature.Status -ne "Valid") {
        & artifact-signing-cli -e https://neu.codesigning.azure.net -a extra-headroom -c extraheadroom -d Headroom $file
        if ($LASTEXITCODE -ne 0) { throw "Signing failed: $file" }
        $signature = Get-AuthenticodeSignature -FilePath $file
    }
    if ($signature.Status -ne "Valid") {
        throw "Invalid Authenticode signature: $file ($($signature.Status))"
    }
}

python scripts/build-windows-signed-runtime.py smoke signed-runtime-work/runtime
if ($LASTEXITCODE -ne 0) { throw "Relocated signed runtime did not start" }

$bundle = "src-tauri/resources/windows-signed-runtime.tar.gz"
$sha = python scripts/build-windows-signed-runtime.py archive signed-runtime-work/runtime $bundle
if ($LASTEXITCODE -ne 0 -or $sha -notmatch '^[a-f0-9]{64}$') { throw "Signed runtime archive failed" }
Add-Content -Path $env:GITHUB_ENV -Value "HEADROOM_SIGNED_RUNTIME_SHA256=$sha"
Write-Host "Signed Windows runtime SHA-256: $sha"
Add-Content -Path $env:GITHUB_STEP_SUMMARY -Value "Signed Windows runtime: $($peFiles.Count) PE files, archive SHA-256 $sha"
Remove-Item -Recurse -Force signed-runtime-work
