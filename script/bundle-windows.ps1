[CmdletBinding()]
Param(
    [Parameter()][Alias('i')][switch]$Install,
    [Parameter()][Alias('h')][switch]$Help,
    [Parameter()][Alias('a')][string]$Architecture
)

# https://stackoverflow.com/questions/57949031/powershell-script-stops-if-program-fails-like-bash-set-o-errexit
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true

$buildSuccess = $false

$OSArchitecture = switch ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture) {
    "X64" { "x86_64" }
    "Arm64" { "aarch64" }
    default { throw "Unsupported architecture" }
}

$Architecture = if ($Architecture) {
    $Architecture
} else {
    $OSArchitecture
}

$CargoOutDir = "./target/$Architecture-pc-windows-msvc/release"

function Get-VSArch {
    param(
        [string]$Arch
    )

    switch ($Arch) {
        "x86_64" { "amd64" }
        "aarch64" { "arm64" }
    }
}

$target = "$Architecture-pc-windows-msvc"

if ($Help) {
    Write-Output "Usage: bundle-windows.ps1 [-Architecture x86_64|aarch64] [-Install] [-Help]"
    Write-Output "Build the installer for Windows."
    Write-Output "Options:"
    Write-Output "  -Architecture, -a Which architecture to build (x86_64 or aarch64)"
    Write-Output "  -Install, -i      Run the installer after building."
    Write-Output "  -Help, -h         Show this help message."
    exit 0
}

$vsDevShell = Get-ChildItem -Path "C:\Program Files\Microsoft Visual Studio\2022\*\Common7\Tools\Launch-VsDevShell.ps1" -ErrorAction SilentlyContinue | Select-Object -First 1
if ($vsDevShell) {
    Push-Location
    & $vsDevShell.FullName -Arch (Get-VSArch -Arch $Architecture) -HostArch (Get-VSArch -Arch $OSArchitecture)
    Pop-Location
}

$workspace = (Resolve-Path "$PSScriptRoot\..").Path
$env:ZED_WORKSPACE = $workspace

Push-Location -Path "$workspace\crates\wu"
$channel = Get-Content "RELEASE_CHANNEL"
$env:ZED_RELEASE_CHANNEL = $channel
$env:RELEASE_CHANNEL = $channel
Pop-Location

if ([string]::IsNullOrWhiteSpace($env:RELEASE_VERSION)) {
    $cargoToml = Get-Content "$workspace\crates\wu\Cargo.toml"
    $env:RELEASE_VERSION = ($cargoToml | Select-String -Pattern '^version = "(.*)"' | Select-Object -First 1).Matches.Groups[1].Value
}

$innoDir = "$workspace\inno\$Architecture"

function PrepareForBundle {
    if (Test-Path "$innoDir") {
        Remove-Item -Path "$innoDir" -Recurse -Force
    }
    New-Item -Path "$innoDir" -ItemType Directory -Force
    Copy-Item -Path "$workspace\crates\wu\resources\windows\*" -Destination "$innoDir" -Recurse -Force
    New-Item -Path "$innoDir\agents" -ItemType Directory -Force
    # node.exe plus the dsh-acp build for this architecture; copying agents\*.exe shipped both
    # dsh-acp.exe and dsh-acp-windows-aarch64.exe in every installer.
    Copy-Item -Path "$workspace\crates\wu\resources\agents\node.exe" -Destination "$innoDir\agents\node.exe" -Force
    $dshAcp = if ($Architecture -eq "aarch64") { "dsh-acp-windows-aarch64.exe" } else { "dsh-acp.exe" }
    Copy-Item -Path "$workspace\crates\wu\resources\agents\$dshAcp" -Destination "$innoDir\agents\dsh-acp.exe" -Force
    if (Test-Path "$workspace\crates\wu\resources\agents\node_modules") {
        Copy-Item -Path "$workspace\crates\wu\resources\agents\node_modules" -Destination "$innoDir\agents" -Recurse -Force
        # Ship only what node loads at runtime: source maps, TypeScript sources and type
        # declarations, markdown docs and test/example trees are about half of node_modules.
        # The copy above is disposable, so pruning it leaves crates\wu\resources\agents intact.
        $bundledNodeModules = "$innoDir\agents\node_modules"
        $prunableFiles = @(Get-ChildItem -Path $bundledNodeModules -Recurse -File |
            Where-Object {
                $_.Extension -in @(".map", ".ts", ".tsbuildinfo", ".md") -or $_.Name -match '\.d\.(m|c)ts$'
            })
        $prunableDirs = @(Get-ChildItem -Path $bundledNodeModules -Recurse -Directory |
            Where-Object { $_.Name -in @("@types", "test", "tests", "__tests__", "examples", "docs") })
        $prunableFiles | Remove-Item -Force
        foreach ($dir in $prunableDirs) {
            if (Test-Path -LiteralPath $dir.FullName) {
                Remove-Item -LiteralPath $dir.FullName -Recurse -Force
            }
        }
    }
    if (Test-Path "$workspace\crates\wu\resources\agents\mcp") {
        Copy-Item -Path "$workspace\crates\wu\resources\agents\mcp" -Destination "$innoDir\agents\mcp" -Recurse -Force
    }
    foreach ($required in @("dsh-acp.exe", "node.exe", "node_modules", "mcp\wu-repo-map\server.js")) {
        if (-not (Test-Path "$innoDir\agents\$required")) {
            throw "Missing agents\$required; the bundle would ship without a working DeepSeek, Pi or repo map server."
        }
    }
    if (Test-Path "$workspace\crates\wu\resources\agents\licenses") {
        Copy-Item -Path "$workspace\crates\wu\resources\agents\licenses" -Destination "$innoDir\agents\licenses" -Recurse -Force
    }
    else {
        throw "Missing crates\wu\resources\agents\licenses; bundled runtimes would ship without their licenses."
    }
    New-Item -Path "$innoDir\make_appx" -ItemType Directory -Force
    New-Item -Path "$innoDir\appx" -ItemType Directory -Force
    New-Item -Path "$innoDir\bin" -ItemType Directory -Force
    New-Item -Path "$innoDir\tools" -ItemType Directory -Force

    rustup target add $target
}

function GenerateLicenses {
    . $PSScriptRoot/generate-licenses.ps1
}

# Inno Setup shows this file before installing, and copies NOTICE next to the
# binary. Both are GPL obligations, so a missing source file is a hard error.
function AssembleLicenseFiles {
    $licensePath = "$innoDir\LICENSE.txt"
    $parts = @("LICENSE-GPL", "LICENSE-APACHE")
    New-Item -Path $licensePath -ItemType File -Value "" -Force | Out-Null
    foreach ($part in $parts) {
        $source = Join-Path $workspace $part
        if (-not (Test-Path $source)) {
            throw "Missing $part; the installer would ship without its license terms."
        }
        Add-Content -Path $licensePath -Value (Get-Content $source -Raw)
        Add-Content -Path $licensePath -Value "`n`n----------------------------------------------------------------------`n"
    }
    if (-not (Test-Path (Join-Path $workspace "NOTICE"))) {
        throw "Missing NOTICE; the installer would ship without its attribution notice."
    }
    Copy-Item -Path (Join-Path $workspace "NOTICE") -Destination "$innoDir\NOTICE" -Force
}

function BuildWuAndItsFriends {
    Write-Output "Building Wu and its friends, for channel: $channel"
    cargo --config .cargo/bundle-config.toml build --release --package wu --package cli --package auto_update_helper --target $target
    Copy-Item -Path ".\$CargoOutDir\IDE.exe" -Destination "$innoDir\IDE.exe" -Force
    Copy-Item -Path ".\$CargoOutDir\cli.exe" -Destination "$innoDir\cli.exe" -Force
    Copy-Item -Path ".\$CargoOutDir\auto_update_helper.exe" -Destination "$innoDir\auto_update_helper.exe" -Force
    switch ($channel) {
        "stable" {
            cargo --config .cargo/bundle-config.toml build --release --features stable --no-default-features --package explorer_command_injector --target $target
        }
        default {
            cargo --config .cargo/bundle-config.toml build --release --package explorer_command_injector --target $target
        }
    }
    Copy-Item -Path ".\$CargoOutDir\explorer_command_injector.dll" -Destination "$innoDir\zed_explorer_command_injector.dll" -Force
}

function BuildRemoteServer {
    Write-Output "Building remote_server for $target"
    cargo --config .cargo/bundle-config.toml build --release --package remote_server --target $target

    $remoteServerSrc = (Resolve-Path ".\$CargoOutDir\remote_server.exe").Path
    $remoteServerDst = "$workspace\target\wu-remote-server-windows-$Architecture.gz"
    Write-Output "Compressing remote_server to $remoteServerDst"

    $input = [System.IO.File]::OpenRead($remoteServerSrc)
    $output = [System.IO.File]::Create($remoteServerDst)
    $gzip = New-Object System.IO.Compression.GZipStream($output, [System.IO.Compression.CompressionLevel]::Optimal)
    try {
        $input.CopyTo($gzip)
    } finally {
        $gzip.Dispose()
        $output.Dispose()
        $input.Dispose()
    }
}

function MakeAppx {
    $manifestFile = "$workspace\crates\explorer_command_injector\AppxManifest.xml"
    Copy-Item -Path "$manifestFile" -Destination "$innoDir\make_appx\AppxManifest.xml"
    $makeAppx = Get-ChildItem -Path "C:\Program Files (x86)\Windows Kits\10\bin\10.*\x64\makeappx.exe" -ErrorAction SilentlyContinue | Sort-Object FullName -Descending | Select-Object -First 1
    if (-not $makeAppx) {
        throw "makeappx.exe not found; install the Windows 10/11 SDK"
    }
    & $makeAppx.FullName pack /d "$innoDir\make_appx" /p "$innoDir\zed_explorer_command_injector.appx" /nv
}

function DownloadAMDGpuServices {
    # If you update the AGS SDK version, please also update the version in `crates/gpui/src/platform/windows/directx_renderer.rs`
    $url = "https://codeload.github.com/GPUOpen-LibrariesAndSDKs/AGS_SDK/zip/refs/tags/v6.3.0"
    $zipPath = ".\AGS_SDK_v6.3.0.zip"
    Invoke-WebRequest -Uri $url -OutFile $zipPath
    Expand-Archive -Path $zipPath -DestinationPath "." -Force
}

function DownloadConpty {
    # TODO: Switch back to the stable channel once Microsoft finally decides to publish v1.26
    $url = "https://github.com/microsoft/terminal/releases/download/v1.25.1912.0/Microsoft.Windows.Console.ConPTY.1.25.260710002-preview.nupkg"
    $zipPath = ".\Microsoft.Windows.Console.ConPTY.1.25.260710002-preview.nupkg"
    Invoke-WebRequest -Uri $url -OutFile $zipPath
    Expand-Archive -Path $zipPath -DestinationPath ".\conpty" -Force
}

function CollectFiles {
    Move-Item -Path "$innoDir\zed_explorer_command_injector.appx" -Destination "$innoDir\appx\zed_explorer_command_injector.appx" -Force
    Move-Item -Path "$innoDir\zed_explorer_command_injector.dll" -Destination "$innoDir\appx\zed_explorer_command_injector.dll" -Force
    Move-Item -Path "$innoDir\cli.exe" -Destination "$innoDir\bin\ide.exe" -Force
    # bin\wu.exe is created as a hard link to bin\ide.exe at install time (see zed.iss),
    # so the installer ships the CLI only once.
    Move-Item -Path "$innoDir\zed.sh" -Destination "$innoDir\bin\ide" -Force
    Copy-Item -Path "$innoDir\bin\ide" -Destination "$innoDir\bin\wu" -Force
    Move-Item -Path "$innoDir\auto_update_helper.exe" -Destination "$innoDir\tools\auto_update_helper.exe" -Force
    if($Architecture -eq "aarch64") {
        New-Item -Type Directory -Path "$innoDir\arm64" -Force
        Move-Item -Path ".\conpty\build\native\runtimes\arm64\OpenConsole.exe" -Destination "$innoDir\arm64\OpenConsole.exe" -Force
        Move-Item -Path ".\conpty\runtimes\win-arm64\native\conpty.dll" -Destination "$innoDir\conpty.dll" -Force
    }
    else {
        New-Item -Type Directory -Path "$innoDir\x64" -Force
        New-Item -Type Directory -Path "$innoDir\arm64" -Force
        Move-Item -Path ".\AGS_SDK-6.3.0\ags_lib\lib\amd_ags_x64.dll" -Destination "$innoDir\amd_ags_x64.dll" -Force
        Move-Item -Path ".\conpty\build\native\runtimes\x64\OpenConsole.exe" -Destination "$innoDir\x64\OpenConsole.exe" -Force
        Move-Item -Path ".\conpty\build\native\runtimes\arm64\OpenConsole.exe" -Destination "$innoDir\arm64\OpenConsole.exe" -Force
        Move-Item -Path ".\conpty\runtimes\win-x64\native\conpty.dll" -Destination "$innoDir\conpty.dll" -Force
    }
}

function BuildInstaller {
    $issFilePath = "$innoDir\zed.iss"
    switch ($channel) {
        "stable" {
            $appId = "{{9BDBDF5C-B3B7-4F80-B831-98CFF7361C8A}"
            # Wu used Zed's AppId before it got its own, so installs from then are registered under it.
            $oldAppId = "2DB0DA96-CA55-49BB-AF4F-64AF36A86712"
            $appIconName = "app-icon"
            # `appDirName` names the install directory. A fresh install lands in {autopf}\IDE and the
            # post-install step deletes a leftover {autopf}\Wu; an upgrade keeps the directory it
            # finds in the previous uninstall entry (Inno's UsePreviousAppDir), so old installs stay in {autopf}\Wu.
            $appDirName = "IDE"
            $appDisplayName = "WuIDE"
            $appSetupName = "IDE-$Architecture"
            # Must match `app_identifier()` in crates/release_channel/src/lib.rs plus the "-Instance-Mutex" suffix
            # used by crates/wu/src/wu/windows_only_instance.rs.
            $appMutex = "Wu-Editor-Stable-Instance-Mutex"
            $appExeName = "IDE"
            $regValueName = "IDE"
            $appUserId = "Farshed.Wu"
            $appShellNameShort = "W&uIDE"
            $appAppxFullName = "Farshed.Wu_1.0.0.0_neutral__japxn1gcva8rg"
        }
        "dev" {
            $appId = "{{3EF9FC05-FE00-4BD0-941C-21F3BA4E6952}"
            $oldAppId = "8357632E-24A4-4F32-BA97-E575B4D1FE5D"
            $appIconName = "app-icon-dev"
            $appDirName = "IDE Dev"
            $appDisplayName = "WuIDE Dev"
            $appSetupName = "IDE-$Architecture"
            $appMutex = "Wu-Editor-Dev-Instance-Mutex"
            $appExeName = "IDE"
            $regValueName = "IDEDev"
            $appUserId = "Farshed.Wu.Dev"
            $appShellNameShort = "W&uIDE Dev"
            $appAppxFullName = "Farshed.Wu_1.0.0.0_neutral__japxn1gcva8rg"
        }
        default {
            Write-Error "can't bundle installer for $channel."
            exit 1
        }
    }

    # The Windows runner image ships ISCC in Program Files; a per-user `winget install JRSoftware.InnoSetup`
    # lands it under LOCALAPPDATA instead, so check both.
    $innoSetupPath = @(
        "C:\Program Files (x86)\Inno Setup 6\ISCC.exe",
        "C:\Program Files\Inno Setup 6\ISCC.exe",
        "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe"
    ) | Where-Object { Test-Path -LiteralPath $_ } | Select-Object -First 1
    if (-not $innoSetupPath) {
        throw "ISCC.exe not found; install Inno Setup 6"
    }

    $definitions = @{
        "AppId"          = $appId
        "OldAppId"       = $oldAppId
        "AppIconName"    = $appIconName
        "OutputDir"      = "$workspace\target"
        "AppSetupName"   = $appSetupName
        "AppDirName"     = $appDirName
        "AppDisplayName" = $appDisplayName
        "RegValueName"   = $regValueName
        "AppMutex"       = $appMutex
        "AppExeName"     = $appExeName
        "ResourcesDir"   = "$innoDir"
        "ShellNameShort" = $appShellNameShort
        "AppUserId"      = $appUserId
        "Version"        = "$env:RELEASE_VERSION"
        "SourceDir"      = "$workspace"
        "AppxFullName"   = $appAppxFullName
    }

    $defs = @()
    foreach ($key in $definitions.Keys) {
        $defs += "/d$key=`"$($definitions[$key])`""
    }

    $innoArgs = @($issFilePath) + $defs

    Write-Host "Running Inno Setup: $innoSetupPath $innoArgs"
    $process = Start-Process -FilePath $innoSetupPath -ArgumentList $innoArgs -NoNewWindow -Wait -PassThru

    if ($process.ExitCode -eq 0) {
        Write-Host "Inno Setup successfully compiled the installer"
        if ($env:GITHUB_ENV) {
            Write-Output "SETUP_PATH=target/$appSetupName.exe" >> $env:GITHUB_ENV
        }
        $script:buildSuccess = $true
    }
    else {
        Write-Host "Inno Setup failed: $($process.ExitCode)"
        $script:buildSuccess = $false
    }
}

Push-Location $workspace
PrepareForBundle
GenerateLicenses
AssembleLicenseFiles
BuildWuAndItsFriends
BuildRemoteServer
MakeAppx
DownloadAMDGpuServices
DownloadConpty
CollectFiles
BuildInstaller
Pop-Location

if ($buildSuccess) {
    Write-Output "Build successful"
    if ($Install) {
        Write-Output "Installing WuIDE..."
        Start-Process -FilePath "$workspace/target/IDE-$Architecture.exe"
    }
    exit 0
}
else {
    Write-Output "Build failed"
    exit 1
}
