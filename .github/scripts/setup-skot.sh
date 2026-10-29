#!/usr/bin/env bash
# setup-skot.sh - download a pre-built skot binary and add it to PATH.
# Called by the composite action (action.yml) at the repository root.
#
# Environment (set by action.yml):
#   INPUT_VERSION  - "latest", "1.2.3", or "v1.2.3"
#   GH_TOKEN       - GitHub token for API requests
#   GH_REPO        - owner/repo (e.g. OmentaElvis/skot)
#   RUNNER_OS      - Linux | macOS | Windows (set by GitHub Actions)
#   RUNNER_ARCH    - X64 | ARM64 | ARM (set by GitHub Actions)
#   RUNNER_TEMP    - temporary directory (set by GitHub Actions)
#   GITHUB_OUTPUT  - output file (set by GitHub Actions)
#   GITHUB_PATH    - path file (set by GitHub Actions)

set -euo pipefail

# ---------------------------------------------------------------------------
# 1. Resolve version
# ---------------------------------------------------------------------------
resolve_version() {
  local requested="$INPUT_VERSION"

  if [[ "$requested" == "latest" ]]; then
    # Query the latest published (non-draft, non-prerelease) release tag
    local tag
    tag=$(gh release view --repo "$GH_REPO" --json tagName --jq '.tagName' 2>/dev/null) || {
      echo "::error::Could not determine the latest skot release. Is the repository public?"
      exit 1
    }
    echo "${tag#v}"
  else
    # Strip optional leading 'v'
    echo "${requested#v}"
  fi
}

# ---------------------------------------------------------------------------
# 2. Map runner platform to release target triple
# ---------------------------------------------------------------------------
resolve_target() {
  case "${RUNNER_OS}-${RUNNER_ARCH}" in
    Linux-X64)    echo "x86_64-unknown-linux-gnu" ;;
    Linux-ARM64)  echo "aarch64-unknown-linux-gnu" ;;
    macOS-X64)    echo "x86_64-apple-darwin" ;;
    macOS-ARM64)  echo "aarch64-apple-darwin" ;;
    Windows-X64)  echo "x86_64-pc-windows-msvc" ;;
    Windows-ARM64) echo "aarch64-pc-windows-msvc" ;;
    *)
      echo "::error::Unsupported platform: ${RUNNER_OS}/${RUNNER_ARCH}"
      exit 1
      ;;
  esac
}

# ---------------------------------------------------------------------------
# 3. Download, extract, and install
# ---------------------------------------------------------------------------
main() {
  local version target archive_name install_dir bin_name download_url

  version=$(resolve_version)
  target=$(resolve_target)
  install_dir="${RUNNER_TEMP}/skot-${version}"
  mkdir -p "$install_dir"

  echo "Installing skot ${version} for ${target}..."

  # Determine archive name and binary name
  if [[ "$RUNNER_OS" == "Windows" ]]; then
    archive_name="skot-v${version}-${target}.zip"
    bin_name="skot.exe"
  else
    archive_name="skot-v${version}-${target}.tar.gz"
    bin_name="skot"
  fi

  download_url="https://github.com/${GH_REPO}/releases/download/v${version}/${archive_name}"

  echo "Downloading ${download_url}..."
  curl --fail --silent --show-error --location \
    --header "Authorization: token ${GH_TOKEN}" \
    --header "Accept: application/octet-stream" \
    --output "${install_dir}/${archive_name}" \
    "$download_url"

  # Extract
  echo "Extracting..."
  if [[ "$RUNNER_OS" == "Windows" ]]; then
    unzip -q -o "${install_dir}/${archive_name}" -d "$install_dir"
  else
    tar xzf "${install_dir}/${archive_name}" -C "$install_dir"
  fi

  chmod +x "${install_dir}/${bin_name}"

  # Verify the binary runs
  "${install_dir}/${bin_name}" --version || true

  # Add to PATH
  echo "${install_dir}" >> "$GITHUB_PATH"

  # Set outputs
  echo "version=${version}" >> "$GITHUB_OUTPUT"
  echo "skot-path=${install_dir}/${bin_name}" >> "$GITHUB_OUTPUT"

  echo "✅ skot ${version} installed to ${install_dir}/${bin_name}"
}

main
