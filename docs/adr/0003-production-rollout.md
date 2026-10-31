# ADR 0003: サービス公開への移行計画（OAuth 実設定・website がサービスになる）

- 状態: 確定（2026-10-31）
- 目標: 「見せるだけの LP」ではなく、Discord ログインから借りられる本物のサービスにする。

## 前提

- 中央サーバー（gpupod-control）: axum + SQLite、serve で HTTP + SSH 中継が同居。
- 公開サイト（apps/site）: Astro 静的。在庫はビルド時取得だが、本番は nginx で振り分け（ADR 0001）。
- 課金はない。プラン（Community/Active/Event）は ADR 0002 の時間・台数上限で運用。

## ステップ 0: 事前に決めるもの（運営工作業）

| 項目 | 値の例 | 用途 |
|---|---|---|
| ドメイン | gpupod.example.org | サイトと API の両方（同一オリジンにする） |
| VPS | ConoHa / さくら / Hetzner | systemd + nginx |
| Discord サーバー | 既存 1551195569429422202 | メンバー限定 |
| Discord アプリ | 新規作成 | OAuth client_id / secret |

必須: ドメインはサイトと API の 1 つのホストで公開する（サブドメインを分けない）。
Cookie の SameSite 制約と CSP を避けるため。

## ステップ 1: Discord Developer Portal の設定

1. https://discord.com/developers/applications → New Application
2. OAuth2 → Add Redirect: https://<ドメイン>/oauth/discord/callback
3. Scopes: identify email guilds
4. Client ID と Client Secret をメモる（secret は再生成可）

## ステップ 2: VPS で gpupod-control を起動

    sudo adduser --system --group --home /opt/gpupod gpupod
    sudo -u gpupod mkdir -m 0700 -p /opt/gpupod/.control-state
    sudo install -m 0755 target/release/gpupod-control /usr/local/bin/

環境変数は /opt/gpupod/.control-state/control.env に置く:

    GPUPOD_CONTROL_STATE=/opt/gpupod/.control-state
    GPUPOD_INSECURE_HTTP=0
    GPUPOD_ALLOWED_HOSTS=gpupod.example.org
    GPUPOD_CSRF_ORIGINS=https://gpupod.example.org
    GPUPOD_RELAY_HOST=gpupod.example.org
    GPUPOD_PUBLIC_BASE_URL=https://gpupod.example.org

    GPUPOD_DISCORD_CLIENT_ID=<Client ID>
    GPUPOD_DISCORD_CLIENT_SECRET=<Client Secret>
    GPUPOD_DISCORD_GUILD_ID=1551195569429422202
    GPUPOD_DISCORD_INVITE_URL=https://discord.gg/xxxx
    GPUPOD_DISCORD_ALLOWED_ROLE_IDS=

    sudo -u gpupod /usr/local/bin/gpupod-control migrate
    sudo -u gpupod /usr/local/bin/gpupod-control init-control
    sudo -u gpupod /usr/local/bin/gpupod-control sync-templates
    sudo -u gpupod /usr/local/bin/gpupod-control create-superuser

systemd ユニットは scripts/systemd/gpupod-control.service を使う。

## ステップ 3: nginx（TLS 端末）

    server {
      listen 443 ssl http2;
      server_name gpupod.example.org;
      ssl_certificate     /etc/letsencrypt/live/gpupod.example.org/fullchain.pem;
      ssl_certificate_key /etc/letsencrypt/live/gpupod.example.org/privkey.pem;
      client_max_body_size 64m;   # benchmark 用

      location /oauth/       { proxy_pass http://127.0.0.1:8000; }
      location /login/       { proxy_pass http://127.0.0.1:8000; }
      location /account/     { proxy_pass http://127.0.0.1:8000; }
      location /api/         { proxy_pass http://127.0.0.1:8000; }
      location /nodes/       { proxy_pass http://127.0.0.1:8000; }
      location / {
        try_files $uri $uri/index.html @gpupod;
      }
      location @gpupod { proxy_pass http://127.0.0.1:8000; }
    }

Cookie は 1 ドメインに閉じるため SameSite は破れない。

## ステップ 4: 在庫のライブ化

初期（第1週）は GitOps 为せ:
1. cron で 5 分ごとに GPUPOD_INVENTORY_URL=http://127.0.0.1:8000/api/public/inventory を fetchする。
2. apps/site/src/data/inventory.json が変化していれば commit と push。
3. GitHub Actions またはローカルで astro build → dist を gh-pages または nginx の docroot に反映。

第 2 週からは直接 GitOps で行わず、キャッシュ付きの API を直接参照する。

## ステップ 5: 提供 PC の参加

    gpupod-agent --state-dir ~/.gpupod daemon --config node-provider.secret.json

USB からは 443 と 19000-19999 の 2 種の outbound 通信のみ。提供側の PCに公開ポートは不要。

## ステップ 6: モデレーション

| 状況 | 対応 |
|---|---|
| 暗号マイニング | node_control で drain、提供者に通知 |
| 不正アカウント | /admin/ から user を deactive |
| OAuth key 漏えい | Discord Developer Portal で regenerate |

## ユーザーに確認が必要なこと

- ドメイン（選定・取得）
- VPS プロバイダ（国と判）
- Discord 招待リンク
- Event プランの運用手順

## 実装の優先順位

1. GET /api/public/inventory を gpupod-control に実装（認証不要、公開 OK の情報だけ）
2. ADR 0002 のプラン上限（Community 1 GPU / 最長 4h+2h）を agent と DB へ実装
3. apps/site の GPUPOD_INVENTORY_URL に本番 URL を差し替え
4. systemd ユニットを scripts/systemd/ と ADR 0003 にあわせて整える
