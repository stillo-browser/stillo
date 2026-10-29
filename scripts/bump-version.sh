#!/usr/bin/env bash
# バージョンを bump して git tag を作成する。
#
# 使い方:
#   ./scripts/bump-version.sh 0.1.12
#
# やること:
#   1. Cargo.toml のバージョンと crates/*/Cargo.toml のクレート間依存バージョンを書き換え
#   2. cargo build でビルド確認（Cargo.lock も更新）
#   3. git commit
#   4. git tag v<version>
#   5. git push origin main + tag
#      → GitHub Actions が自動で publish を実行する

set -euo pipefail

NEW_VERSION="${1:-}"
if [ -z "$NEW_VERSION" ]; then
  echo "Usage: $0 <version>  (例: $0 0.1.12)"
  exit 1
fi

if ! [[ "$NEW_VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "Error: version must be X.Y.Z (got: $NEW_VERSION)"
  exit 1
fi

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CARGO_TOML="$ROOT/Cargo.toml"

# 未コミットの変更がバージョン bump のコミットに混入したり、逆に取り残されたりするのを防ぐ
if ! git -C "$ROOT" diff --quiet || ! git -C "$ROOT" diff --cached --quiet; then
  echo "Error: working tree has uncommitted changes"
  exit 1
fi
BRANCH=$(git -C "$ROOT" rev-parse --abbrev-ref HEAD)
if [ "$BRANCH" != "main" ]; then
  echo "Error: must be on main (current: $BRANCH)"
  exit 1
fi

CURRENT=$(grep '^version' "$CARGO_TOML" | head -1 | sed 's/.*"\(.*\)".*/\1/')
echo "Current version : $CURRENT"
echo "New version     : $NEW_VERSION"

if [ "$CURRENT" = "$NEW_VERSION" ]; then
  echo "Already at $NEW_VERSION, nothing to do."
  exit 0
fi

# Cargo.toml を書き換え
sed -i "s/^version = \"$CURRENT\"/version = \"$NEW_VERSION\"/" "$CARGO_TOML"
# クレート間の依存バージョンも更新（例: version = "0.1" → "0.2"）
OLD_MINOR=$(echo "$CURRENT" | sed "s/\.[0-9]*$//")
NEW_MINOR=$(echo "$NEW_VERSION" | sed "s/\.[0-9]*$//")
# stillo-* の path 依存の行だけを対象にする。全置換すると同じバージョン指定を持つ外部クレートまで書き換わる
OLD_MINOR_RE=${OLD_MINOR//./\\.}
find "$ROOT/crates" -name Cargo.toml -exec \
  sed -i -E "/^stillo-[a-z]+ = \{ path = /s/version = \"$OLD_MINOR_RE\"/version = \"$NEW_MINOR\"/" {} \;

# ビルドして Cargo.lock を更新
echo "Building..."
cargo build --manifest-path "$CARGO_TOML" --quiet

# コミット
git -C "$ROOT" add Cargo.toml Cargo.lock "crates/*/Cargo.toml"
git -C "$ROOT" commit -m "chore: bump version to $NEW_VERSION"

# タグ作成 & push
git -C "$ROOT" tag "v$NEW_VERSION"
git -C "$ROOT" push origin main "v$NEW_VERSION"

echo "Done! GitHub Actions will publish v$NEW_VERSION to crates.io."
