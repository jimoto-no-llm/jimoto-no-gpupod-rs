#!/bin/sh
# デモ用: 提供PCを1台登録し、エージェント用の設定ファイルを書き出す。
#
# 本番の登録は管理パネル（/nodes/）から行ってください。このスクリプトは
# docker-compose のデモを 1 コマンドで立ち上げるためだけに使います。
#
# 環境変数:
#   GPUPOD_CONTROL_STATE            中央の状態ディレクトリ
#   GPUPOD_PROVIDER_STATE           提供エージェントの状態ディレクトリ
#   GPUPOD_PROVIDER_CONFIG          書き出す設定ファイル
#   GPUPOD_DEMO_PROVIDER_NAME       パネルに表示する提供PC名
#   GPUPOD_DEMO_PROVIDER_URL        中央の API URL（提供エージェントから見た URL）
#   GPUPOD_DEMO_PROVIDER_RELAY_HOST 中継ホスト（提供エージェントから見たホスト名）
#   GPUPOD_DEMO_PROVIDER_RELAY_PORT 中継制御ポート
set -eu

umask 077

state="${GPUPOD_CONTROL_STATE:-/var/lib/gpupod/control}"
provider_state="${GPUPOD_PROVIDER_STATE:-/var/lib/gpupod/provider}"
config="${GPUPOD_PROVIDER_CONFIG:-$provider_state/node.secret.json}"
name="${GPUPOD_DEMO_PROVIDER_NAME:-Mock provider (Docker demo)}"
url="${GPUPOD_DEMO_PROVIDER_URL:-http://127.0.0.1:8000}"
relay_host="${GPUPOD_DEMO_PROVIDER_RELAY_HOST:-127.0.0.1}"
relay_port="${GPUPOD_DEMO_PROVIDER_RELAY_PORT:-19000}"

db="$state/control.sqlite3"
host_key="$state/relay-host-key"

for command in sqlite3 openssl ssh-keygen sha256sum; do
  command -v "$command" >/dev/null 2>&1 || {
    echo "[gpupod] 必要なコマンドがありません: $command" >&2
    exit 1
  }
done

[ -f "$db" ] || { echo "[gpupod] 中央のDBがありません: $db" >&2; exit 1; }
[ -f "$host_key" ] || { echo "[gpupod] 中継ホスト鍵がありません: $host_key" >&2; exit 1; }

mkdir -p "$provider_state"
chmod 700 "$provider_state" 2>/dev/null || true

existing="$(sqlite3 -cmd '.timeout 5000' "$db" "select count(*) from portal_node where name = '$name';")"
if [ -s "$config" ] && [ "$existing" != "0" ]; then
  echo "[gpupod] デモ提供PCは登録済みです（$config）"
  exit 0
fi
if [ "$existing" != "0" ]; then
  echo "[gpupod] 設定が無い古い登録を削除して作り直します: $name"
  sqlite3 -cmd '.timeout 5000' "$db" "delete from portal_node where name = '$name';"
fi

node_hex="$(openssl rand -hex 16)"
token="$(openssl rand -base64 48 | tr -d '\n=' | tr '+/' '-_')"
digest="$(printf '%s' "$token" | sha256sum | cut -d' ' -f1)"
node_uuid="$(printf '%s' "$node_hex" | sed 's/^\(.\{8\}\)\(.\{4\}\)\(.\{4\}\)\(.\{4\}\)\(.\{12\}\)$/\1-\2-\3-\4-\5/')"
public_key="$(ssh-keygen -y -f "$host_key" | tr -d '\n')"

sqlite3 -cmd '.timeout 5000' "$db" "\
INSERT INTO portal_node (id, name, token_hash, enabled, desired_enabled, last_seen, snapshot, statistics, \
                         previous_token_hash, previous_token_until, pending_token_hash, pending_token_until, \
                         pending_rotation_id, rotation_id, update_channel, benchmark_download_after, \
                         benchmark_upload_after) \
VALUES ('$node_hex', '$name', '$digest', 1, 1, NULL, '{}', '{}', '', NULL, '', NULL, NULL, NULL, 'stable', NULL, NULL);"

cat > "$config" <<EOF
{
  "url": "$url",
  "node_id": "$node_uuid",
  "token": "$token",
  "relay_host": "$relay_host",
  "relay_port": $relay_port,
  "relay_host_key": "$public_key"
}
EOF
chmod 600 "$config"

echo "[gpupod] デモ提供PCを登録しました: $name ($node_uuid)"
echo "[gpupod] 提供エージェント設定: $config"
