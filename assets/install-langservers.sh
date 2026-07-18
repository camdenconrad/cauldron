#!/usr/bin/env bash
# Install the language servers Cauldron speaks to (beyond clangd / rust-analyzer, which come
# from the distro / rustup):
#
#   pyright                      -> pyright-langserver --stdio        (Python)
#   typescript-language-server   -> typescript-language-server --stdio (JS/TS/TSX; needs the
#   typescript                      typescript package for tsserver itself)
#   vscode-langservers-extracted -> vscode-css-language-server --stdio
#                                   vscode-html-language-server --stdio
#
# Everything goes to the npm global prefix (~/.npm-global on this box). Safe to re-run.
#
# PATH: interactive shells already have ~/.npm-global/bin on PATH via the shell profile. If a
# GUI-launched Cauldron doesn't inherit it, cauldron-lsp's spawn falls back to the absolute
# ~/.npm-global/bin/<server> path automatically — no PATH surgery required.
set -euo pipefail

PREFIX="$(npm config get prefix)"
echo "npm prefix: $PREFIX"

npm install -g pyright typescript-language-server typescript vscode-langservers-extracted

echo
echo "installed binaries:"
ok=1
for b in pyright-langserver typescript-language-server vscode-css-language-server vscode-html-language-server; do
    if p="$(command -v "$b" 2>/dev/null)"; then
        printf '  %-30s %s\n' "$b" "$p"
    elif [ -x "$PREFIX/bin/$b" ]; then
        printf '  %-30s %s (NOT on PATH — cauldron uses its built-in fallback)\n' "$b" "$PREFIX/bin/$b"
    else
        printf '  %-30s MISSING\n' "$b"
        ok=0
    fi
done

if [ "$ok" -eq 0 ]; then
    echo "some servers failed to install" >&2
    exit 1
fi
echo
echo "done. If $PREFIX/bin is not on your PATH, add:  export PATH=\"$PREFIX/bin:\$PATH\""
