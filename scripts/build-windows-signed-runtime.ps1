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
if ($unsigned.Count -ne 0) {
    # artifact-signing-cli 0.11.0 logs in, fetches the signing dlib and writes
    # metadata.json, then runs signtool once per file (~1.6s each: 684 files
    # took 18 minutes). Sign one file through it for that setup, then the rest
    # with signtool directly: many files per call, several calls at once.
    & artifact-signing-cli -e https://neu.codesigning.azure.net -a extra-headroom -c extraheadroom -d Headroom $unsigned[0]
    if ($LASTEXITCODE -ne 0) { throw "Signing failed for $($unsigned[0])" }
    $config = Join-Path $HOME ".artifact-signing-cli"
    $chunks = @(for ($i = 1; $i -lt $unsigned.Count; $i += 40) {
        , @($unsigned[$i..([Math]::Min($i + 39, $unsigned.Count - 1))])
    })
    $failed = @($chunks | ForEach-Object -ThrottleLimit 8 -Parallel {
        $signtool = "C:\Program Files (x86)\Windows Kits\10\bin\10.0.26100.0\x64\signtool.exe"
        $config = $using:config
        & $signtool sign /fd SHA256 /tr http://timestamp.acs.microsoft.com /td SHA256 /dlib "$config\lib\bin\x64\Azure.CodeSigning.Dlib.dll" /dmdf "$config\metadata.json" /d Headroom @_ | Out-Host
        if ($LASTEXITCODE -ne 0) { $_[0] }
    })
    if ($failed.Count -ne 0) { throw "signtool failed for the batches starting at: $($failed -join ', ')" }
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
# The app's runtime generation: every build input, so any change reaches installed users.
$key = python scripts/build-windows-signed-runtime.py cache-key
if ($LASTEXITCODE -ne 0) { throw "Signed runtime cache key failed" }
Add-Content -Path $env:GITHUB_ENV -Value "HEADROOM_SIGNED_RUNTIME_KEY=$key"
Write-Host "Signed Windows runtime SHA-256: $sha"
Add-Content -Path $env:GITHUB_STEP_SUMMARY -Value "Signed Windows runtime: $($peFiles.Count) PE files, archive SHA-256 $sha"
Remove-Item -Recurse -Force signed-runtime-work
