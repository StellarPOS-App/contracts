#!/usr/bin/env bash
# Bumps the contract version across all relevant files.
# Updates the CHANGELOG.md and ensures version metadata is consistent.
set -euo pipefail

PROJECT_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VERSION_FILE="$PROJECT_ROOT/CHANGELOG.md"

if [[ ! -f "$VERSION_FILE" ]]; then
    echo "CHANGELOG.md not found at $VERSION_FILE" >&2
    exit 1
fi

# Read current version from the first line of CHANGELOG (assuming format: ## [x.y.z] - YYYY-MM-DD)
CURRENT_VERSION=$(head -1 "$VERSION_FILE" | grep -oP '\[\K[0-9]+(\.[0-9]+)+' || echo "0.0.0")

# Parse major, minor, patch
IFS='.' read -r major minor patch <<<"$CURRENT_VERSION"

# Increment patch version
patch=$((patch + 1))
NEW_VERSION="${major}.${minor}.${patch}"

echo "Bumping version from $CURRENT_VERSION to $NEW_VERSION"

# Update CHANGELOG.md header
sed -i "s/## \[$CURRENT_VERSION\]/## [$NEW_VERSION] - $(date -u +%Y-%m-%d)/" "$VERSION_FILE"

# Update version in all contract source files' metadata comments
# This is a placeholder - in practice, version metadata is embedded in the wasm
# via soroban_sdk::contractmeta! macros. A full version bump would typically
# involve rebuilding and redeploying contracts.
echo "Version bump complete: $NEW_VERSION"
echo "Please rebuild contracts and update CHANGELOG.md with release notes."