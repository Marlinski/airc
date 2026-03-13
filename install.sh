#!/usr/bin/env sh

set -e

# AIRC Installer
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/Marlinski/airc/main/install.sh | sh
#   curl -fsSL https://raw.githubusercontent.com/Marlinski/airc/main/install.sh | AIRC_RELEASE=v0.2.0 sh
#   curl -fsSL https://raw.githubusercontent.com/Marlinski/airc/main/install.sh | sh -s -- --install-dir /usr/local/bin

REPO="Marlinski/airc"
INSTALL_DIR="${AIRC_INSTALL_DIR:-$HOME/.local/bin}"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

log_info()    { printf "${BLUE}[INFO]${NC} %s\n" "$1"; }
log_success() { printf "${GREEN}[OK]${NC} %s\n" "$1"; }
log_warn()    { printf "${YELLOW}[WARN]${NC} %s\n" "$1"; }
log_error()   { printf "${RED}[ERROR]${NC} %s\n" "$1"; }

# Detect OS + architecture -> asset suffix
detect_platform() {
    local os arch

    case "$(uname -s)" in
        Linux*)  os="linux" ;;
        Darwin*) os="macos" ;;
        *)
            log_error "Unsupported OS: $(uname -s)"
            exit 1
            ;;
    esac

    case "$(uname -m)" in
        x86_64|amd64)  arch="x86_64" ;;
        arm64|aarch64) arch="aarch64" ;;
        *)
            log_error "Unsupported architecture: $(uname -m)"
            exit 1
            ;;
    esac

    echo "${os}-${arch}"
}

# Fetch JSON from GitHub API (uses curl or wget)
fetch() {
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL "$1"
    elif command -v wget >/dev/null 2>&1; then
        wget -qO- "$1"
    else
        log_error "Neither curl nor wget found. Please install one of them."
        exit 1
    fi
}

# Download a file
download() {
    local url="$1" dest="$2"
    log_info "Downloading $(basename "$dest")..."
    if command -v curl >/dev/null 2>&1; then
        curl -fSL -o "$dest" "$url"
    else
        wget -O "$dest" "$url"
    fi
}

# Get the release JSON (latest or specific tag)
get_release() {
    local tag="${AIRC_RELEASE:-latest}"
    local api_url

    if [ "$tag" = "latest" ]; then
        api_url="https://api.github.com/repos/${REPO}/releases/latest"
    else
        api_url="https://api.github.com/repos/${REPO}/releases/tags/${tag}"
    fi

    fetch "$api_url"
}

# Extract download URL for a given asset name from release JSON
extract_url() {
    local json="$1" name="$2"
    echo "$json" | grep -o "\"browser_download_url\":[[:space:]]*\"[^\"]*${name}[^\"]*\"" | cut -d'"' -f4
}

main() {
    log_info "Installing AIRC..."

    local platform
    platform=$(detect_platform)
    log_info "Detected platform: ${platform}"

    # Fetch release metadata
    local release_json
    release_json=$(get_release)

    if [ -z "$release_json" ]; then
        log_error "Failed to fetch release information from GitHub"
        exit 1
    fi

    # Extract tag name for display
    local tag_name
    tag_name=$(echo "$release_json" | grep -o '"tag_name":[[:space:]]*"[^"]*"' | cut -d'"' -f4)
    log_info "Release: ${tag_name:-unknown}"

    # Resolve download URLs for both binaries
    local airc_url aircd_url
    airc_url=$(extract_url "$release_json" "airc-${platform}")
    aircd_url=$(extract_url "$release_json" "aircd-${platform}")

    if [ -z "$airc_url" ]; then
        log_error "No airc binary found for platform: ${platform}"
        log_error "Available assets:"
        echo "$release_json" | grep -o '"name":[[:space:]]*"[^"]*"' | cut -d'"' -f4
        exit 1
    fi

    if [ -z "$aircd_url" ]; then
        log_warn "No aircd (server) binary found for platform: ${platform}"
        log_warn "Only the client will be installed."
    fi

    # Create install directory
    if [ ! -d "$INSTALL_DIR" ]; then
        log_info "Creating ${INSTALL_DIR}"
        mkdir -p "$INSTALL_DIR"
    fi

    # Download and install airc
    local tmp_airc="/tmp/airc-$$"
    download "$airc_url" "$tmp_airc"
    mv "$tmp_airc" "${INSTALL_DIR}/airc"
    chmod +x "${INSTALL_DIR}/airc"
    log_success "Installed airc -> ${INSTALL_DIR}/airc"

    # Download and install aircd (if available)
    if [ -n "$aircd_url" ]; then
        local tmp_aircd="/tmp/aircd-$$"
        download "$aircd_url" "$tmp_aircd"
        mv "$tmp_aircd" "${INSTALL_DIR}/aircd"
        chmod +x "${INSTALL_DIR}/aircd"
        log_success "Installed aircd -> ${INSTALL_DIR}/aircd"
    fi

    # PATH check
    update_path

    log_success "AIRC ${tag_name:-} installed successfully!"
    echo ""
    log_info "Quick start:"
    echo "  airc connect irc.example.com --nick mybot --join '#general'"
    echo "  airc say '#general' 'Hello, world!'"
    echo "  airc fetch"
    echo ""
    log_info "For MCP integration (Claude Desktop, OpenCode, Cursor):"
    echo "  airc mcp"
}

update_path() {
    case ":${PATH}:" in
        *":${INSTALL_DIR}:"*) return ;;
    esac

    log_warn "${INSTALL_DIR} is not in your PATH."

    local shell_rc=""
    if [ -n "${ZSH_VERSION:-}" ] || [ "$(basename "${SHELL:-}")" = "zsh" ]; then
        shell_rc="$HOME/.zshrc"
    elif [ -n "${BASH_VERSION:-}" ] || [ "$(basename "${SHELL:-}")" = "bash" ]; then
        if [ -f "$HOME/.bash_profile" ]; then
            shell_rc="$HOME/.bash_profile"
        else
            shell_rc="$HOME/.bashrc"
        fi
    fi

    if [ -n "$shell_rc" ] && [ -f "$shell_rc" ]; then
        echo "export PATH=\"\$PATH:${INSTALL_DIR}\"" >> "$shell_rc"
        log_success "Added ${INSTALL_DIR} to PATH in ${shell_rc}"
        log_warn "Run 'source ${shell_rc}' or restart your terminal."
    else
        log_warn "Add this to your shell profile:"
        echo "  export PATH=\"\$PATH:${INSTALL_DIR}\""
    fi
}

# Parse args
while [ $# -gt 0 ]; do
    case $1 in
        --install-dir)
            INSTALL_DIR="$2"
            shift 2
            ;;
        --help)
            echo "AIRC Installer"
            echo ""
            echo "Usage: curl -fsSL https://raw.githubusercontent.com/Marlinski/airc/main/install.sh | sh"
            echo ""
            echo "Options:"
            echo "  --install-dir DIR   Install to DIR (default: \$HOME/.local/bin)"
            echo "  --help              Show this help"
            echo ""
            echo "Environment variables:"
            echo "  AIRC_RELEASE        Release tag to install (default: latest)"
            echo "  AIRC_INSTALL_DIR    Same as --install-dir"
            exit 0
            ;;
        *)
            log_error "Unknown option: $1"
            exit 1
            ;;
    esac
done

main
