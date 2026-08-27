<#
.SYNOPSIS
    Builds Jot and stages a distributable folder and zip.

.DESCRIPTION
    Produces target/dist/Jot-<version>/ containing the executable and the
    licence files it is required to ship with, then zips it.

    This deliberately stops short of claiming to produce a *signed* artifact.
    Windows SmartScreen will warn on an unsigned binary, and the only fix is a
    real code-signing certificate — see the Signing section below. A script that
    quietly produced an unsigned zip and called it a release would be lying
    about the one thing that matters to whoever downloads it.

.PARAMETER Toolchain
    Rust toolchain to build with. Defaults to the one pinned in
    rust-toolchain.toml; pass it explicitly if RUSTUP_TOOLCHAIN is set in your
    environment, because that variable overrides the pin.

.PARAMETER CertificateThumbprint
    Optional. When given, signs the executable with that certificate from the
    current user's store before packaging.

.EXAMPLE
    ./scripts/package.ps1
    ./scripts/package.ps1 -Toolchain 1.97.1
    ./scripts/package.ps1 -CertificateThumbprint ABC123...
#>
[CmdletBinding()]
param(
    [string]$Toolchain,
    [string]$CertificateThumbprint
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Push-Location $root
try {
    # Every cargo call builds one complete argument array and splats it whole.
    # Anything cleverer breaks: a PowerShell function's parameter binder eats
    # cargo's `-p` and `--`, and splatting a partial array ahead of literal
    # arguments hands rustup an empty toolchain name.
    # [string[]] matters: a bare `if` returning a one-element array unrolls it to
    # a string, and `$prefix + $rest` would then concatenate text instead of
    # building an argument list.
    [string[]]$prefix = if ($Toolchain) { @("+$Toolchain") } else { @() }
    function Get-CargoArgv([string[]]$Rest) { $script:prefix + $Rest }

    Write-Host 'Checking formatting, lints and tests before building…'

    $argv = Get-CargoArgv @('fmt', '--all', '--check')
    & cargo @argv
    if ($LASTEXITCODE -ne 0) { throw 'cargo fmt --check failed' }

    $argv = Get-CargoArgv @('clippy', '--all-targets', '--locked', '--', '-D', 'warnings')
    & cargo @argv
    if ($LASTEXITCODE -ne 0) { throw 'clippy failed' }

    $argv = Get-CargoArgv @('test', '--locked')
    & cargo @argv
    if ($LASTEXITCODE -ne 0) { throw 'tests failed' }

    Write-Host 'Building release…'
    $argv = Get-CargoArgv @('build', '--release', '-p', 'jot', '--locked')
    & cargo @argv
    if ($LASTEXITCODE -ne 0) { throw 'release build failed' }

    $argv = Get-CargoArgv @('metadata', '--no-deps', '--format-version', '1')
    $manifest = & cargo @argv | ConvertFrom-Json
    $version = ($manifest.packages | Where-Object { $_.name -eq 'jot' }).version
    $exe = Join-Path $root 'target/release/jot.exe'
    if (-not (Test-Path $exe)) { throw "missing $exe" }

    if ($CertificateThumbprint) {
        Write-Host "Signing with certificate $CertificateThumbprint…"
        $cert = Get-ChildItem -Path "Cert:\CurrentUser\My\$CertificateThumbprint" -ErrorAction Stop
        Set-AuthenticodeSignature -FilePath $exe -Certificate $cert `
            -TimestampServer 'http://timestamp.digicert.com' -HashAlgorithm SHA256 | Out-Null
    } else {
        Write-Warning 'No certificate given: the binary is UNSIGNED and SmartScreen will warn on first run.'
    }

    $stage = Join-Path $root "target/dist/Jot-$version"
    if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
    New-Item -ItemType Directory -Path $stage -Force | Out-Null

    Copy-Item $exe $stage
    foreach ($file in @('LICENSE', 'THIRD_PARTY_NOTICES.md', 'README.md')) {
        Copy-Item (Join-Path $root $file) $stage
    }

    $zip = Join-Path $root "target/dist/Jot-$version-windows-x64.zip"
    if (Test-Path $zip) { Remove-Item -Force $zip }
    Compress-Archive -Path (Join-Path $stage '*') -DestinationPath $zip

    $signature = (Get-AuthenticodeSignature $exe).Status
    # The winget manifest needs this, and computing it by hand is a step people
    # get wrong quietly.
    $hash = (Get-FileHash -Path $zip -Algorithm SHA256).Hash
    Write-Host ''
    Write-Host "Staged:  $stage"
    Write-Host "Zip:     $zip"
    Write-Host "SHA256:  $hash"
    Write-Host "Signed:  $signature"
    Write-Host ''
    Write-Host 'Jot is portable: it needs no installer, keeps its data in'
    Write-Host '%LOCALAPPDATA%\Jot, and adds itself to startup only when the user'
    Write-Host 'turns that on in Settings.'
}
finally {
    Pop-Location
}
