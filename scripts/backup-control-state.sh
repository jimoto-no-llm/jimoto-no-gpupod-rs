#!/bin/sh
# 中央の状態（DB・中継ホスト鍵・セッション鍵・設定）をバックアップする。
#
#   sudo ./scripts/backup-control-state.sh
#   sudo ./scripts/backup-control-state.sh --keep 30 --output /var/backups/gpupod
#
# バックアップは秘密情報を含むため 0600 で作成します。公開ストレージへ置かないでください。
set -eu

state="${GPUPOD_CONTROL_STATE:-/var/lib/gpupod/control}"
output="${GPUPOD_BACKUP_DIR:-/var/backups/gpupod}"
keep=14

while [ "$#" -gt 0 ]; do
  case "$1" in
    --keep)
      keep="${2:?--keep には日数を指定してください}"
      shift 2
      ;;
    --output)
      output="${2:?--output にはディレクトリを指定してください}"
      shift 2
      ;;
    -h | --help)
      sed -n '2,12p' "$0"
      exit 0
      ;;
    *)
      echo "不明な引数: $1" >&2
      exit 2
      ;;
  esac
done

command -v sqlite3 >/dev/null 2>&1 || { echo "sqlite3 が必要です" >&2; exit 1; }
command -v tar >/dev/null 2>&1 || { echo "tar が必要です" >&2; exit 1; }

[ -d "$state" ] || { echo "状態ディレクトリがありません: $state" >&2; exit 1; }
db="$state/control.sqlite3"
[ -f "$db" ] || { echo "DB がありません: $db" >&2; exit 1; }

umask 077
install -d -m 0700 "$output"

stamp="$(date +%Y%m%d-%H%M%S)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# 実行中の DB から一貫性のあるコピーを作る
sqlite3 "$db" ".timeout 5000" ".backup '$work/control.sqlite3'"
for item in relay-host-key secret-key control.env; do
  if [ -f "$state/$item" ]; then
    cp -p "$state/$item" "$work/$item"
  fi
done

archive="$output/gpupod-control-$stamp.tar.gz"
tar -czf "$archive" -C "$work" .
chmod 600 "$archive"

echo "バックアップを作成しました: $archive"

# 古いバックアップを削除（ファイル名のタイムスタンプ順 = 古い順）
set --
for file in "$output"/gpupod-control-*.tar.gz; do
  [ -e "$file" ] || continue
  set -- "$@" "$file"
done
total=$#
while [ "$total" -gt "$keep" ]; do
  oldest="$1"
  shift
  total=$((total - 1))
  rm -f "$oldest"
  echo "古いバックアップを削除: $oldest"
done

echo "保持数: $keep 件"
