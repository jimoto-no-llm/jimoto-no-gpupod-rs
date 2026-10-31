#!/bin/bash
# 中央サーバーの死活確認。cron やイベント前チェックに使う。
#
#   ./scripts/health-check.sh https://gpu.example.org
#   ./scripts/health-check.sh https://gpu.example.org gpupod-relay 19000
#
# 引数: <base-url> [中継ポートを確認するホスト] [中継制御ポート]
# 終了コード: 0 = 正常, 1 = 異常
set -u

base_url="${1:-http://127.0.0.1:8000}"
relay_host="${2:-}"
relay_port="${3:-19000}"
failed=0

check() {
  if "$@"; then
    return 0
  fi
  failed=1
  return 1
}

echo "[health] healthz: $base_url/healthz"
if check curl -fsS --max-time 5 "$base_url/healthz"; then
  echo
else
  echo "[health] NG: healthz に到達できません" >&2
fi

echo "[health] top page"
code="$(curl -sS -o /dev/null -w '%{http_code}' --max-time 5 "$base_url/" || echo 000)"
echo "  HTTP $code"
case "$code" in
  200 | 302) ;;
  *) failed=1 ;;
esac

if [ -n "$relay_host" ]; then
  echo "[health] relay: $relay_host:$relay_port"
  if timeout 3 bash -c "exec 3<>/dev/tcp/$relay_host/$relay_port" 2>/dev/null; then
    echo "  OK"
  else
    echo "  NG: 中継ポートに接続できません" >&2
    failed=1
  fi
fi

if [ "$failed" -eq 0 ]; then
  echo "[health] すべて正常です"
else
  echo "[health] 異常があります" >&2
fi
exit "$failed"
