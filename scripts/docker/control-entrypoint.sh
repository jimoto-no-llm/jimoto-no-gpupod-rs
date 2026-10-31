#!/bin/sh
# 中央サーバーの起動前処理とサーブ。
# 環境変数:
#   GPUPOD_CONTROL_STATE           状態ディレクトリ（既定 /var/lib/gpupod/control）
#   GPUPOD_TEMPLATES               テンプレートカタログ（既定 /opt/gpupod/examples/templates.json）
#   GPUPOD_BIND                    Web のバインド先（既定 0.0.0.0:8000）
#   GPUPOD_SUPERUSER_USERNAME      初期管理者のユーザー名（既定 admin）
#   GPUPOD_SUPERUSER_PASSWORD      初期管理者のパスワード（空なら作成しない）
#   GPUPOD_DEMO_PROVIDER           1 ならデモ用の提供PCを自動登録する
set -eu

state="${GPUPOD_CONTROL_STATE:-/var/lib/gpupod/control}"
catalog="${GPUPOD_TEMPLATES:-/opt/gpupod/examples/templates.json}"
bind="${GPUPOD_BIND:-0.0.0.0:8000}"

mkdir -p "$state"
chmod 700 "$state" 2>/dev/null || true

echo "[gpupod] migrate"
gpupod-control migrate

echo "[gpupod] init-control"
gpupod-control init-control

if [ -f "$catalog" ]; then
  echo "[gpupod] sync-templates ($catalog)"
  gpupod-control sync-templates "$catalog"
else
  echo "[gpupod] テンプレートカタログが見つかりません: $catalog" >&2
fi

# 初期管理者。既に存在する場合は作成しない（create-superuser は重複で失敗する）。
username="${GPUPOD_SUPERUSER_USERNAME:-admin}"
password="${GPUPOD_SUPERUSER_PASSWORD:-}"
if [ -n "$password" ]; then
  case "$username" in
    *[!A-Za-z0-9_.@-]*)
      echo "[gpupod] GPUPOD_SUPERUSER_USERNAME に使えない文字があります: $username" >&2
      exit 1
      ;;
  esac
  count="$(sqlite3 "$state/control.sqlite3" "select count(*) from auth_user where username = '$username';")"
  if [ "$count" = "0" ]; then
    echo "[gpupod] 初期管理者を作成します: $username"
    printf '%s\n%s\n%s\n' "$username" "$password" "$password" | gpupod-control create-superuser
  fi
fi

if [ "${GPUPOD_DEMO_PROVIDER:-0}" = "1" ]; then
  /usr/local/bin/gpupod-bootstrap-provider.sh
fi

echo "[gpupod] serve --bind $bind"
exec gpupod-control serve --bind "$bind"
