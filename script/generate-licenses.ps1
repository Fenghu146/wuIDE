$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true

$CARGO_ABOUT_VERSION="0.8.2"
$outputFile=$args[0] ? $args[0] : "$(Get-Location)/assets/licenses.md"
$templateFile="script/licenses/template.md.hbs"

New-Item -Path "$outputFile" -ItemType File -Value "" -Force

@(
    "# ###### THEME LICENSES ######\n"
    Get-Content assets/themes/LICENSES
    "\n# ###### ICON LICENSES ######\n"
    Get-Content assets/icons/LICENSES
    "\n# ###### CODE LICENSES ######\n"
) | Add-Content -Path $outputFile

$needsInstall = $false
try {
    $versionOutput = cargo about --version
    if (-not ($versionOutput -match "cargo-about $CARGO_ABOUT_VERSION")) {
        $needsInstall = $true
    } else {
        Write-Host "cargo-about@$CARGO_ABOUT_VERSION is already installed"
    }
} catch {
    $needsInstall = $true
}

if ($needsInstall) {
    Write-Host "Installing cargo-about@$CARGO_ABOUT_VERSION..."
    cargo install "cargo-about@$CARGO_ABOUT_VERSION"
}

Write-Host "Generating cargo licenses"

$licensesTimeoutSeconds = if ($env:LICENSES_TIMEOUT_SECONDS) { [int]$env:LICENSES_TIMEOUT_SECONDS } else { 1200 }
$failFlag = $env:ALLOW_MISSING_LICENSES ? "--fail" : ""
$args = @('about', 'generate', $failFlag, '-c', 'script/licenses/zed-licenses.toml', $templateFile, '-o', $outputFile) | Where-Object { $_ }
# `cargo about` gathers licenses through `cargo metadata`, which fetches the
# lockfile's git dependencies with its output captured: a stalled fetch prints
# nothing at all (see the note in script/generate-licenses). Bound it, so a stall
# fails the bundle in 20 minutes rather than hanging until the job timeout.
$argumentList = ($args | ForEach-Object { if ($_ -match '\s') { "`"$_`"" } else { $_ } }) -join ' '
$cargoAbout = Start-Process -FilePath 'cargo' -ArgumentList $argumentList -NoNewWindow -PassThru
if (-not $cargoAbout.WaitForExit($licensesTimeoutSeconds * 1000)) {
    try { $cargoAbout.Kill($true) } catch { $cargoAbout.Kill() }
    Write-Host "licenses generation stalled; see the cargo about output above (killed after ${licensesTimeoutSeconds}s)"
    throw "cargo about generate stalled for ${licensesTimeoutSeconds}s"
}
if ($cargoAbout.ExitCode -ne 0) {
    throw "cargo about generate failed with exit code $($cargoAbout.ExitCode)"
}

Write-Host "Applying replacements"
$replacements = @{
    '&quot;' = '"'
    '&#x27;' = "'"
    '&#x3D;' = '='
    '&#x60;' = '`'
    '&lt;'   = '<'
    '&gt;'   = '>'
}
$content = Get-Content $outputFile
foreach ($find in $replacements.keys) {
    $content = $content -replace $find, $replacements[$find]
}
$content | Set-Content $outputFile

Write-Host "generate-licenses completed. See $outputFile"
