#!/usr/bin/env bash
# Scaffold for `read-narrowly` — runs OUTSIDE the agent's sandbox, in the run's empty workspace, only
# under `--scaffold`. It materialises a MOUNTED project: `docli.toml` (a mount that names a
# workspace and grants nothing), the AGENTS.md contract the CLI installs, and project files that
# make the local path tempting. No credentials and no mirror — the harness's HOME is throwaway —
# so `docli search` fails here and the orientation's own fallback (`search_notes` over MCP) is the
# path under test; the MCP server is the suite's mock under `mocks/docli/`.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../../../.." && pwd)"
cp "$REPO/AGENTS.md" ./AGENTS.md
cat > docli.toml <<'TOML'
# docli.toml - the mount table for docli-cli (committed; names workspaces, grants nothing).
server = "https://docli.ru"
mcp_label = "docli"

[[mount]]
workspace = "cd2f1093-4219-4d68-8d2c-dfe7d5125b72"
TOML
cp -R "$HERE/project/." .
