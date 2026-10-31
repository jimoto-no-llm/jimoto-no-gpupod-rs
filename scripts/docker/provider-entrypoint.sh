#!/bin/sh
# 提供エージェント（デモでは GPU をダミーにした mock モード）の起動。
# 初回だけ貸出枠と受付 ON を設定し、その後は常駐デーモンを起動します。
set -eu

state="${GPUPOD_AGENT_STATE:-/var/lib/gpupod/provider}"
config="${GPUPOD_PROVIDER_CONFIG:-$state/node.secret.json}"
templates="${GPUPOD_TEMPLATES:-/opt/gpupod/examples/templates.json}"
socket="${GPUPOD_DOCKER_SOCKET:-/var/run/docker.sock}"
memory_gib="${GPUPOD_DEMO_MEMORY_GIB:-8}"
disk_gib="${GPUPOD_DEMO_DISK_GIB:-100}"

# 中央がダウンロードした node-*.secret.json が置かれるまで待つ。
i=0
while [ ! -s "$config" ]; do
  i=$((i + 1))
  if [ "$i" -gt 120 ]; then
    echo "[gpupod] 提供PCの設定が見つかりません: $config" >&2
    echo "[gpupod] 管理パネルから提供PCを登録し、node-*.secret.json を配置してください。" >&2
    exit 1
  fi
  sleep 1
done
chmod 600 "$config" 2>/dev/null || true

agent() {
  gpupod-agent --mock-gpu --state-dir "$state" --templates "$templates" --docker-socket "$socket" "$@"
}

if [ ! -f "$state/.demo-setup-done" ]; then
  echo "[gpupod] 貸出枠を設定します（RAM ${memory_gib} GiB / ディスク ${disk_gib} GiB）"
  agent resources --memory-gib "$memory_gib" --disk-gib "$disk_gib" --default-memory-gib 2 --default-disk-gib 25
  echo "[gpupod] 受付を ON にします"
  agent on
  : > "$state/.demo-setup-done"
fi

echo "[gpupod] daemon を起動します"
# POSIX sh（dash）の exec はシェル関数を実行できないため、バイナリを直接 exec する。
exec gpupod-agent --mock-gpu --state-dir "$state" --templates "$templates" \
  --docker-socket "$socket" daemon --config "$config"
