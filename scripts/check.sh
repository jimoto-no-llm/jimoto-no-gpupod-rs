#!/bin/sh
# ローカルの検証をまとめて実行する（CI と同じコマンド）。
#
#   ./scripts/check.sh
#
# fmt は差分があっても失敗扱いにせず、修正方法を表示します。
set -u

cd "$(dirname "$0")/.." || exit 1
export PATH="${HOME}/.cargo/bin:${PATH}"

status=0

echo "== cargo fmt --all --check =="
if cargo fmt --all --check; then
  echo "OK"
else
  echo "整形の差分があります。cargo fmt --all で適用してください。"
  status=1
fi

echo
echo "== cargo clippy --workspace --all-targets --all-features =="
if cargo clippy --workspace --all-targets; then
  echo "OK"
else
  status=1
fi

echo
echo "== cargo test --workspace --all-features =="
if cargo test --workspace --no-fail-fast; then
  echo "OK"
else
  status=1
fi

echo
if [ "$status" -eq 0 ]; then
  echo "すべて成功しました。"
else
  echo "失敗した項目があります。上の出力を確認してください。"
fi
exit "$status"
