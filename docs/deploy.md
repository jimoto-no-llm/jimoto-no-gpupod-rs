# デプロイガイド（VPS 1 台 + systemd + nginx + TLS）

イベントで使う中央サーバーを、1 台の VPS に立てる手順です。
提供 PC は参加者の手元にあり、中央へ外向きに接続します。所要時間は 30〜60 分です。

> 対象: Ubuntu 24.04 LTS（Debian 系ならほぼ同じ）/ systemd / nginx / Let's Encrypt。
> 手順中の `gpu.example.org` は自分のドメインに置き換えてください。

## 1. 構成

| コンポーネント | 置き場所 | 公開 |
| --- | --- | --- |
| gpupod-control（WebUI / API） | 127.0.0.1:8000 | nginx 経由で 443/TCP |
| gpupod-control（SSH 中継） | 0.0.0.0:19000 | 19000〜19999/TCP |
| 提供 PC（gpupod-agent） | 参加者の PC | 外向きのみ（公開ポート不要） |
| 利用者 | ブラウザー + SSH クライアント | 443 / 割当ポート |

```text
利用者 ──HTTPS(443)──▶ nginx ──▶ gpupod-control (127.0.0.1:8000)
   │
   └──SSH(194xx)─────▶ gpupod-control 中継 (19000-19999) ◀──提供PC（外向き接続）
```

中継は 1 貸出あたり最大 4 ポートを 19001〜19999 からランダムに割り当てます。
19000 は提供 PC が中継制御に使うポートで、利用者は接続しません。

## 2. ホストの準備

```bash
sudo apt-get update
sudo apt-get install -y nginx certbot python3-certbot-nginx sqlite3 curl ufw

# 専用ユーザー（ログイン不可）
sudo useradd --system --home-dir /var/lib/gpupod --shell /usr/sbin/nologin gpupod
sudo install -d -m 0700 -o gpupod -g gpupod /var/lib/gpupod/control
sudo install -d -m 0755 -o root -g gpupod /etc/gpupod
sudo install -d -m 0755 /usr/local/share/gpupod
```

DNS で `gpu.example.org` の A レコード（または AAAA）を VPS の IP に向けておきます。

## 3. バイナリの配置

リリースバイナリは 3 つです（gpupod-control / gpupod-agent / gpupod-tools）。
VPS 上でビルドする場合は、メモリ 2 GiB 以上を確保してください（スワップ推奨）。

```bash
# 方法 A: VPS 上でビルド
sudo apt-get install -y build-essential pkg-config
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
git clone https://github.com/jimoto-no-llm/jimoto-no-gpupod-rs.git
cd jimoto-no-gpupod-rs
cargo build --release --locked --workspace --bins

# 方法 B: 手元でクロスビルドして転送（VPS は glibc 互換の Linux が必要）
# scp target/release/gpupod-{control,agent,tools} user@gpu.example.org:/tmp/

sudo install -m 0755 target/release/gpupod-control /usr/local/bin/
sudo install -m 0755 target/release/gpupod-agent   /usr/local/bin/
sudo install -m 0755 target/release/gpupod-tools   /usr/local/bin/
sudo install -m 0644 examples/templates.json /usr/local/share/gpupod/templates.json
```

Docker イメージを使う場合は `Dockerfile` をビルドして `gpupod-control` を実行できますが、
このガイドはバイナリ + systemd の構成を前提にしています（Docker 構成は
`docker-compose.yml` のローカルデモ専用です）。

## 4. 中央の初期化

環境変数は `/etc/gpupod/control.env` にまとめます（0600）。`examples/control.env` が雛形です。

```bash
sudo install -m 0640 -o root -g gpupod examples/control.env /etc/gpupod/control.env
sudo editor /etc/gpupod/control.env
```

`/etc/gpupod/control.env` の最低限の設定:

```dotenv
GPUPOD_CONTROL_STATE=/var/lib/gpupod/control
GPUPOD_ALLOWED_HOSTS=gpu.example.org
GPUPOD_CSRF_ORIGINS=https://gpu.example.org
GPUPOD_RELAY_HOST=gpu.example.org
GPUPOD_RELAY_PORT=19000
GPUPOD_RELAY_PORT_MIN=19001
GPUPOD_RELAY_PORT_MAX=19999
GPUPOD_RELAY_BIND_HOST=0.0.0.0
GPUPOD_TRUST_PROXY=1
GPUPOD_DISCORD_CLIENT_ID=...
GPUPOD_DISCORD_CLIENT_SECRET=...
GPUPOD_PUBLIC_BASE_URL=https://gpu.example.org
GPUPOD_DISCORD_GUILD_ID=1551195569429422202
GPUPOD_DISCORD_INVITE_URL=https://discord.gg/xxxxxxx
```

- `GPUPOD_TRUST_PROXY=1` は nginx が `X-Forwarded-Proto` を上書きする構成で必要です。
- `GPUPOD_INSECURE_HTTP=1` は TLS なしの検証時だけ使います。本番では設定しません。
- Discord の Client Secret は `control.env` だけに置き、リポジトリへコミットしないでください。

初期化（初回のみ。2 回目以降はスキップして構いません）:

```bash
sudo -u gpupod -H sh -c '
  set -a; . /etc/gpupod/control.env; set +a
  gpupod-control migrate
  gpupod-control init-control
  gpupod-control sync-templates /usr/local/share/gpupod/templates.json
  gpupod-control create-superuser
'
```

`create-superuser` は対話式です。作成した管理者でログインし、運営アカウントを管理画面から作ります。
`init-control` は中継の SSH ホスト鍵を `$GPUPOD_CONTROL_STATE/relay-host-key` に作ります。
**このファイルと DB を失うと、登録済み提供 PC の機材キーと中継の信頼が切れます。** バックアップを取ってください。

## 5. systemd で常駐させる

同梱のユニットを使う場合:

```bash
sudo ./scripts/install-systemd.sh            # /etc/systemd/system へ配置 + daemon-reload
sudo systemctl enable --now gpupod-control.service
systemctl status gpupod-control.service
journalctl -u gpupod-control.service -f
```

ユニットの内容は [scripts/systemd/gpupod-control.service](../scripts/systemd/gpupod-control.service) を参照してください。
`serve` は Web と中継を 1 プロセスで起動します。分離したい場合は `gpupod-relay.service` を使ってください。

`gpupod-tools systemd-units` も使えますが、生成される ExecStart は Python 版（`.venv/bin/python`）前提です。
Rust 版では上記の同梱ユニットを使用してください。

## 6. nginx と TLS

`examples/nginx.conf` をベースに、ドメインと証明書パスを置き換えます。

```bash
sudo install -m 0644 examples/nginx.conf /etc/nginx/sites-available/gpupod
sudo ln -sf /etc/nginx/sites-available/gpupod /etc/nginx/sites-enabled/gpupod
sudo rm -f /etc/nginx/sites-enabled/default
```

Rust 版は CSS などの静的ファイルをバイナリに埋め込んでいるため、Python 版にあった
`location /static/` の alias は不要です（あっても動作に影響しません）。
アップロード上限とタイムアウトは回線測定 API 用に `examples/nginx.conf` の
`/api/nodes/<id>/benchmark/` 設定を残してください。

```bash
sudo nginx -t && sudo systemctl reload nginx
sudo certbot --nginx -d gpu.example.org --redirect
sudo certbot renew --dry-run
```

## 7. ファイアウォール

```bash
sudo ufw allow OpenSSH
sudo ufw allow 443/tcp
sudo ufw allow 19000:19999/tcp
sudo ufw enable
sudo ufw status verbose
```

- 8000/TCP は公開しません（`127.0.0.1` のみ）。
- 提供 PC 側は外向き接続のみで、公開ポートは不要です。

## 8. 動作確認

```bash
curl -fsS https://gpu.example.org/healthz          # {"status":"ok"}
curl -fsS https://gpu.example.org/ | head -5       # LP が返る
ss -lntp | grep -E ':(8000|19000)\b'               # Web と中継が待ち受け
```

ブラウザーで管理者ログイン → 「提供PC管理」から提供 PC を登録し、
ダウンロードした `node-*.secret.json` を提供者へ渡します。提供者が
`gpupod-agent daemon` を起動すると、数十秒でオンライン表示になります。

利用者アカウントは Discord ログインで自動作成されます。運営用のアカウントは
`/admin/` から作成し、通常の利用者にはスタッフ権限を付けません。

## 9. 運用

- バックアップ: `sudo ./scripts/backup-control-state.sh`（cron 例は [docs/operations.md](operations.md)）
- ヘルスチェック: `./scripts/health-check.sh https://gpu.example.org`
- 更新: 新しいバイナリを `/usr/local/bin` へ配置 → `sudo systemctl restart gpupod-control`。
  DB マイグレーションを伴う場合は、先にバックアップを取り、`gpupod-control migrate` を実行します。
- ロールバック: 旧バイナリを保持しておき、差し替えて再起動します。DB はマイグレーション後に
  旧版で動かない可能性があるため、バックアップからの復元を用意してください。

## 10. つまずきやすい点

| 症状 | 確認すること |
| --- | --- |
| パネルが 400/403 | `GPUPOD_ALLOWED_HOSTS` / `GPUPOD_CSRF_ORIGINS` がドメインと一致しているか、`GPUPOD_TRUST_PROXY=1` |
| ログイン後に HTTP に戻る | `X-Forwarded-Proto` が nginx で上書きされているか、`GPUPOD_PUBLIC_BASE_URL` |
| Discord ログインが失敗 | Redirect URI が `https://gpu.example.org/oauth/discord/callback` か、Client Secret の設定 |
| 提供 PC がオンラインにならない | 提供者のログ（401/403 は機材キー不一致）、`https` 到達性、時計ずれ |
| SSH だけつながらない | 19000〜19999/TCP の開放、提供 PC の外向き接続、`GPUPOD_RELAY_HOST` の表示値 |
| 中継ポートが枯渇する | 同時貸出数と 19001〜19999 の範囲、終了した貸出の後片付け |
| systemd が起動直後に落ちる | `journalctl`、`control.env` の権限（0600 で `gpupod` が読めるか）、状態ディレクトリの所有者 |

## 11. 1 台構成の限界

- 中継の帯域は VPS の回線に依存します。SSH の作業データ転送が多いイベントでは、
  回線速度と 19000〜19999 のポート数が同時接続数の上限になります。
- Web と中継が同居するため、Web の再起動で中継も切れます。貸出中のコンテナは提供 PC 側で維持されますが、
  再起動中は SSH が切れます。イベント中は安易に再起動しないでください。
- 1 台構成では冗長化がありません。イベント中に VPS が落ちた場合は、
  [docs/operations.md](operations.md) の「障害時の確認」に従って復旧してください。
