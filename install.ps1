#Requires -Version 5.1
<#
.SYNOPSIS
    Installs gib CLI tool on Windows.
.DESCRIPTION
    Downloads and installs the latest version of gib for Windows.
    Run with: irm https://raw.githubusercontent.com/Theryston/gib/main/install.ps1 | iex
#>

$ErrorActionPreference = "Stop"

$Repo = "Theryston/gib"
$BinaryName = "gib"
$InstallDir = "$env:LOCALAPPDATA\Programs\gib"

# Colors
function Write-ColorOutput($ForegroundColor) {
    $fc = $host.UI.RawUI.ForegroundColor
    $host.UI.RawUI.ForegroundColor = $ForegroundColor
    if ($args) {
        Write-Output $args
    }
    $host.UI.RawUI.ForegroundColor = $fc
}

# Banner
Write-Host ""
Write-Host "   _____ _____ ____  " -ForegroundColor Cyan
Write-Host "  / ____|_   _|  _ \ " -ForegroundColor Cyan
Write-Host " | |  __  | | | |_) |" -ForegroundColor Cyan
Write-Host " | | |_ | | | |  _ < " -ForegroundColor Cyan
Write-Host " | |__| |_| |_| |_) |" -ForegroundColor Cyan
Write-Host "  \_____|_____|____/ " -ForegroundColor Cyan
Write-Host ""
Write-Host "Installing $BinaryName..." -ForegroundColor Cyan
Write-Host ""

# Detect architecture
function Get-Architecture {
    $arch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture
    switch ($arch) {
        "X64" { return "x86_64" }
        "Arm64" { return "aarch64" }
        default { return "unknown" }
    }
}

$Arch = Get-Architecture

Write-Host "Detected Architecture: " -NoNewline -ForegroundColor Yellow
Write-Host $Arch

if ($Arch -eq "unknown") {
    Write-Host "Error: Unsupported architecture" -ForegroundColor Red
    Write-Host "This installer supports x86_64 and ARM64 only."
    exit 1
}

$Target = "$Arch-pc-windows-msvc"
Write-Host "Target: " -NoNewline -ForegroundColor Yellow
Write-Host $Target
Write-Host ""

# Get latest release
Write-Host "Fetching latest release..." -ForegroundColor Cyan

try {
    $Release = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest" -UseBasicParsing
    $Version = $Release.tag_name
} catch {
    Write-Host "Error: Could not fetch latest release" -ForegroundColor Red
    Write-Host "Please check your internet connection or try again later."
    exit 1
}

Write-Host "Latest version: " -NoNewline -ForegroundColor Green
Write-Host $Version
Write-Host ""

# Build download URL
$DownloadUrl = "https://github.com/$Repo/releases/download/$Version/$BinaryName-$Target.zip"

Write-Host "Downloading $BinaryName..." -ForegroundColor Cyan
Write-Host "URL: $DownloadUrl" -ForegroundColor Yellow
Write-Host ""

# Create temp directory
$TempDir = Join-Path ([System.IO.Path]::GetTempPath()) ([System.Guid]::NewGuid().ToString())
New-Item -ItemType Directory -Path $TempDir -Force | Out-Null

try {
    # Download
    $ZipPath = Join-Path $TempDir "$BinaryName.zip"
    Invoke-WebRequest -Uri $DownloadUrl -OutFile $ZipPath -UseBasicParsing

    # Extract
    Write-Host "Extracting..." -ForegroundColor Cyan
    Expand-Archive -Path $ZipPath -DestinationPath $TempDir -Force

    # Create install directory
    if (!(Test-Path $InstallDir)) {
        New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
    }

    # Move binary
    Write-Host "Installing to $InstallDir..." -ForegroundColor Cyan
    $BinaryPath = Join-Path $TempDir "$BinaryName.exe"
    Copy-Item -Path $BinaryPath -Destination "$InstallDir\$BinaryName.exe" -Force

    # Add to PATH if not already there
    $UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if ($UserPath -notlike "*$InstallDir*") {
        Write-Host "Adding to PATH..." -ForegroundColor Cyan
        $NewPath = "$UserPath;$InstallDir"
        [Environment]::SetEnvironmentVariable("Path", $NewPath, "User")
        $env:Path = "$env:Path;$InstallDir"
        Write-Host "Added $InstallDir to user PATH" -ForegroundColor Green
    } else {
        Write-Host "$InstallDir is already in PATH" -ForegroundColor Green
    }

    Write-Host ""
    Write-Host "✓ $BinaryName installed successfully!" -ForegroundColor Green
    Write-Host ""
    Write-Host "Run " -NoNewline
    Write-Host "$BinaryName --help" -ForegroundColor Yellow -NoNewline
    Write-Host " to get started."
    Write-Host ""
    Write-Host "NOTE: " -ForegroundColor Yellow -NoNewline
    Write-Host "You may need to restart your terminal for PATH changes to take effect."
    Write-Host ""
    Write-Host "Thank you for installing $BinaryName!" -ForegroundColor Green

} catch {
    Write-Host "Error during installation: $_" -ForegroundColor Red
    exit 1
} finally {
    # Cleanup
    if (Test-Path $TempDir) {
        Remove-Item -Path $TempDir -Recurse -Force -ErrorAction SilentlyContinue
    }
}
