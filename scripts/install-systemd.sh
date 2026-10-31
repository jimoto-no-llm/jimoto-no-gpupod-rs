#!/bin/sh
# systemd ユニットを /etc/systemd/system へ配置する。
#
#   sudo ./scripts/install-systemd.sh            # 配置のみ
#   sudo ./scripts/install-systemd.sh --enable   # 配置 + gpupod-control を有効化
#
# 配置後にユーザー名・パス・グループを見直してください（scripts/systemd/*.service）。
set -eu

if [ "$(id -u)" -ne 0 ]; then
  echo "root で実行してください（sudo ./scripts/install-systemd.sh）" >&2
  exit 1
fi

root="$(cd "$(dirname "$0")/.." && pwd)"
source_dir="$root/scripts/systemd"

[ -d "$source_dir" ] || { echo "ユニットが見つかりません: $source_dir" >&2; exit 1; }

install -d -m 0755 /etc/systemd/system
for unit in "$source_dir"/*.service; do
  install -m 0644 "$unit" /etc/systemd/system/
  echo "installed: /etc/systemd/system/$(basename "$unit")"
done

systemctl daemon-reload

if [ "${1:-}" = "--enable" ]; then
  systemctl enable --now gpupod-control.service
  echo "gpupod-control.service を有効化しました"
fi

cat <<'EOS'
配置しました。次のコマンドで状態を確認できます。
  systemctl status gpupod-control.service
  journalctl -u gpupod-control.service -f

提供 PC では gpupod-provider.service を使います（ユーザーとグループを確認してから有効化）。
EOS
