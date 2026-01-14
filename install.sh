#!/bin/bash
set -e

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

REPO="Theryston/gib"
BINARY_NAME="gib"
INSTALL_DIR="/usr/local/bin"

echo -e "${BLUE}"
echo "   _____ _____ ____  "
echo "  / ____|_   _|  _ \ "
echo " | |  __  | | | |_) |"
echo " | | |_ | | | |  _ < "
echo " | |__| |_| |_| |_) |"
echo "  \_____|_____|____/ "
echo -e "${NC}"
echo -e "${BLUE}Installing ${BINARY_NAME}...${NC}"
echo ""

# Detect OS
detect_os() {
    case "$(uname -s)" in
        Linux*)     echo "linux";;
        Darwin*)    echo "macos";;
        *)          echo "unknown";;
    esac
}

# Detect architecture
detect_arch() {
    case "$(uname -m)" in
        x86_64|amd64)   echo "x86_64";;
        aarch64|arm64)  echo "aarch64";;
        *)              echo "unknown";;
    esac
}

OS=$(detect_os)
ARCH=$(detect_arch)

echo -e "${YELLOW}Detected OS:${NC} $OS"
echo -e "${YELLOW}Detected Architecture:${NC} $ARCH"
echo ""

if [ "$OS" = "unknown" ]; then
    echo -e "${RED}Error: Unsupported operating system${NC}"
    echo "This installer supports Linux and macOS only."
    echo "For Windows, use: irm https://raw.githubusercontent.com/$REPO/main/install.ps1 | iex"
    exit 1
fi

if [ "$ARCH" = "unknown" ]; then
    echo -e "${RED}Error: Unsupported architecture${NC}"
    echo "This installer supports x86_64 and aarch64 (ARM64) only."
    exit 1
fi

# Build target string
if [ "$OS" = "linux" ]; then
    TARGET="${ARCH}-unknown-linux-gnu"
elif [ "$OS" = "macos" ]; then
    TARGET="${ARCH}-apple-darwin"
fi

echo -e "${YELLOW}Target:${NC} $TARGET"
echo ""

# Get latest release version
echo -e "${BLUE}Fetching latest release...${NC}"
LATEST_RELEASE=$(curl -s "https://api.github.com/repos/$REPO/releases/latest" | grep '"tag_name":' | sed -E 's/.*"([^"]+)".*/\1/')

if [ -z "$LATEST_RELEASE" ]; then
    echo -e "${RED}Error: Could not fetch latest release${NC}"
    echo "Please check your internet connection or try again later."
    exit 1
fi

echo -e "${GREEN}Latest version:${NC} $LATEST_RELEASE"
echo ""

# Build download URL
DOWNLOAD_URL="https://github.com/$REPO/releases/download/$LATEST_RELEASE/${BINARY_NAME}-${TARGET}.tar.gz"

echo -e "${BLUE}Downloading ${BINARY_NAME}...${NC}"
echo -e "${YELLOW}URL:${NC} $DOWNLOAD_URL"
echo ""

# Create temp directory
TMP_DIR=$(mktemp -d)
trap "rm -rf $TMP_DIR" EXIT

# Download and extract
if command -v curl &> /dev/null; then
    curl -fsSL "$DOWNLOAD_URL" -o "$TMP_DIR/gib.tar.gz"
elif command -v wget &> /dev/null; then
    wget -q "$DOWNLOAD_URL" -O "$TMP_DIR/gib.tar.gz"
else
    echo -e "${RED}Error: Neither curl nor wget found${NC}"
    exit 1
fi

# Extract
tar -xzf "$TMP_DIR/gib.tar.gz" -C "$TMP_DIR"

# Check if we need sudo
NEED_SUDO=false
if [ ! -w "$INSTALL_DIR" ]; then
    NEED_SUDO=true
fi

# Install
echo -e "${BLUE}Installing to ${INSTALL_DIR}...${NC}"

if [ "$NEED_SUDO" = true ]; then
    echo -e "${YELLOW}Root privileges required. You may be prompted for your password.${NC}"
    sudo mv "$TMP_DIR/$BINARY_NAME" "$INSTALL_DIR/$BINARY_NAME"
    sudo chmod +x "$INSTALL_DIR/$BINARY_NAME"
else
    mv "$TMP_DIR/$BINARY_NAME" "$INSTALL_DIR/$BINARY_NAME"
    chmod +x "$INSTALL_DIR/$BINARY_NAME"
fi

# Verify installation
if command -v $BINARY_NAME &> /dev/null; then
    echo ""
    echo -e "${GREEN}✓ ${BINARY_NAME} installed successfully!${NC}"
    echo ""
    echo -e "Run ${YELLOW}${BINARY_NAME} --help${NC} to get started."
else
    echo ""
    echo -e "${YELLOW}⚠ ${BINARY_NAME} was installed to ${INSTALL_DIR}${NC}"
    echo ""
    echo "If the command is not found, make sure ${INSTALL_DIR} is in your PATH."
    echo "You can add it by running:"
    echo ""
    echo -e "  ${YELLOW}export PATH=\"\$PATH:${INSTALL_DIR}\"${NC}"
    echo ""
    echo "Add this line to your ~/.bashrc, ~/.zshrc, or equivalent to make it permanent."
fi

echo ""
echo -e "${GREEN}Thank you for installing ${BINARY_NAME}!${NC}"
