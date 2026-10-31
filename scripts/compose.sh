#!/bin/sh
# docker-compose を DOCKER_GID 付きで実行する薄いラッパー。
#
#   ./scripts/compose.sh up -d
#   ./scripts/compose.sh logs -f control
#   ./scripts/compose.sh down
#
# Docker ソケットのグループ ID を自動で設定します（提供エージェントが
# 非 root のままホストの Docker を操作するために必要）。
set -eu

cd "$(dirname "$0")/.."

socket="${DOCKER_SOCKET:-/var/run/docker.sock}"
if [ -S "$socket" ]; then
  export DOCKER_GID="${DOCKER_GID:-$(stat -c '%g' "$socket" 2>/dev/null || stat -f '%g' "$socket")}"
fi

exec docker compose "$@"
