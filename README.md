# Jimoto GPU Pod (Rust)

[![CI](https://github.com/jimoto-no-llm/jimoto-no-gpupod-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/jimoto-no-llm/jimoto-no-gpupod-rs/actions/workflows/ci.yml)
[![Dependency audit](https://github.com/jimoto-no-llm/jimoto-no-gpupod-rs/actions/workflows/audit.yml/badge.svg)](https://github.com/jimoto-no-llm/jimoto-no-gpupod-rs/actions/workflows/audit.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](https://github.com/jimoto-no-llm/jimoto-no-gpupod-rs)

Linux の提供者 PC の GPU を、承認済み Docker テンプレートで貸し出すためのソフトウェアです。
中央サーバーに認証付き WebUI、提供 PC にアウトバウンド接続のエージェントを置きます。

これは Python/Django 版（jimoto-no-llm/jimoto-no-gpupod）の Rust 移植版です。

## 構成

| crate | 役割 | Python 版の対応 |
| --- | --- | --- |
| crates/gpupod-common | 共通の型・検証・資源計算・同期プロトコル | provider_agent/models.py, resources.py |
| crates/gpupod-control | 中央サーバー（WebUI + API + SSH 中継） | Django の portal / control_panel |
| crates/gpupod-agent | 提供PC側エージェント（常駐・Docker 操作・中継） | provider_agent/ |
| crates/gpupod-tools | 補助ツール（診断・設定生成・更新等） | helpers/ |
| apps/site | 公開サイト（Astro・静的生成） | Django のテンプレートのうち公開ページ |

中央は axum + SQLite（rusqlite）+ サーバーサイドセッション、中継は russh（pure Rust SSH）、
提供側のコンテナ操作は bollard（Docker Engine API）、状態保存は SQLite + flock、
ホスト統計は sysinfo + /proc、署名付き更新は ed25519 + SHA-256 の簡易 TUF 風実装を使用します。

## 言語の分担

| 領域 | 言語 |
| --- | --- |
| 提供エージェント・SSH 中継・CLI | Rust |
| 中央 API（貸出・ノード・認証・状態） | Rust |
| 公開サイト・ダッシュボード | TypeScript（Astro + TanStack） |
| ベンチ集計・分析 | Python |
| スキーマ・運用手順 | SQL / Bash |

判断の理由と移行手順は [ADR 0001](docs/adr/0001-web-stack.md) にあります。

## 公開サイト（apps/site）

```bash
cd apps/site
pnpm install
pnpm run build      # dist/ に静的ファイルを出力
```

ビルド時に在庫を取り込みます。

| 環境変数 | 用途 |
| --- | --- |
| GPUPOD_INVENTORY_URL | 在庫 JSON の取得先（未設定なら空在庫でビルド） |
| GPUPOD_DISCORD_INVITE_URL | 招待リンク（未設定なら「運営に確認」表示） |
| GPUPOD_DISCORD_GUILD_ID | 参加必須のサーバー ID |
| PUBLIC_APP_BASE_URL | ログインボタンの遷移先（Rust の WebUI） |

`dist/` を nginx などで配信し、`/login/` や `/console/` は Rust の WebUI へプロキシします。

## ビルド

```bash
cargo build --release
cargo test
```

バイナリは target/release/ に生成されます: gpupod-control / gpupod-agent / gpupod-tools

## GPU がなくても試す

Linux、Docker Engine が必要です。

```bash
target/release/gpupod-control migrate
target/release/gpupod-control init-control
target/release/gpupod-control sync-templates examples/templates.json
target/release/gpupod-control create-superuser
target/release/gpupod-control serve
```

別プロセスで提供エージェントを起動します:

```bash
target/release/gpupod-agent --mock-gpu --state-dir .provider-state \
  --templates examples/templates.json on
target/release/gpupod-agent --mock-gpu --state-dir .provider-state \
  --templates examples/templates.json daemon --config node-provider.secret.json
```

または補助ツールからデモを起動します:

```bash
target/release/gpupod-tools demo --pytorch
```

`http://127.0.0.1:8000/` を開きます（GPUPOD_INSECURE_HTTP=1 で HTTP を許可）。
デモは **GPU 情報と GPU のコンテナへの割当だけをダミーにします**。Docker コンテナ、作業用ボリューム、
SSH、HTTP 認証、中央中継、返却時の削除は実際に動作します。状態は .demo-state/ に保存し、
Ctrl+C で停止します。実行中のコンテナはエージェント停止時にも維持されるため、
作業終了時はパネルから返却してください。

## テンプレート

カタログは examples/templates.json（Python 版と共通）。利用者は登録済み ID から選びます。
利用要求から任意のイメージ、ホストのマウント、Docker オプションは指定できません。
中央と提供 PC のカタログの指紋（Python 版 json.dumps(sort_keys=True) とバイト一致する正規化 JSON の SHA-256）
が一致したテンプレートだけを貸し出します。

```bash
target/release/gpupod-tools build-images --pytorch cpu cuda rocm xpu
```

各テンプレートには 22、8888、6006、8080 の TCP ポートを用意します。
標準で起動するサービスは公開鍵認証の SSH のみです。SSH ユーザーは pod、作業場所は /workspace です。

## 中央サーバー

```bash
target/release/gpupod-control migrate
target/release/gpupod-control init-control
target/release/gpupod-control sync-templates
target/release/gpupod-control create-superuser
target/release/gpupod-control serve
```

WebUI は HTTPS のリバースプロキシ経由で公開します。examples/nginx.conf、examples/control.env が設定例です。
GPUPOD_ALLOWED_HOSTS、GPUPOD_CSRF_ORIGINS、GPUPOD_RELAY_HOST を実際のドメインに設定します。
GPUPOD_TRUST_PROXY=1 はプロキシが X-Forwarded-Proto を上書きする構成で使います。
ローカル HTTP のみで試す場合は GPUPOD_INSECURE_HTTP=1 を指定します。

管理者でログインし、「提供PC管理」から提供 PC を登録します。ダウンロードされた node-….secret.json を
その PC に渡します。利用者アカウントは簡易管理画面（/admin/）から作成します。

中央の状態、セッション署名鍵、中継 SSH ホスト鍵は .control-state/ に永続化します。
GPUPOD_CONTROL_STATE で保存先を変えられます。

## ログイン（Discord OAuth）

利用者は Discord でログインします。**参加は誰でも可能**ですが、
**Discord サーバー（ID 1551195569429422202）への参加が必須**です。
未参加のままログインしようとすると、参加手順と招待リンクを表示する案内ページになります。

Discord 側の準備:

1. Discord Developer Portal でアプリケーションを作成する
2. OAuth2 → Redirects に `{GPUPOD_PUBLIC_BASE_URL}/oauth/discord/callback` を登録する
3. スコープは `identify email guilds`
4. Client ID と Client Secret を環境変数へ設定する

```bash
GPUPOD_DISCORD_CLIENT_ID=...
GPUPOD_DISCORD_CLIENT_SECRET=...
GPUPOD_PUBLIC_BASE_URL=https://gpu.example.org
GPUPOD_DISCORD_GUILD_ID=1551195569429422202
GPUPOD_DISCORD_INVITE_URL=https://discord.gg/xxxxxxx
```

`GPUPOD_DISCORD_GUILD_ID` は既定で `1551195569429422202` です。
`GPUPOD_DISCORD_ALLOWED_ROLE_IDS` を設定すると、指定ロールの保持も必須になります。
Client ID / Secret が未設定でもサーバーは起動し、パスワードログインのみ利用できます。

## 公開サイト

ログインしていない利用者には LP を、ログイン済みにはダッシュボードを同じ `/` で表示します。

| パス | 内容 |
| --- | --- |
| `/` | LP（未ログイン）/ ダッシュボード（ログイン済み） |
| `/pricing/` | 料金（当面無料） |
| `/providers/` | 提供者募集 |
| `/how/` | 使い方 |
| `/terms/` | 利用規約（叩き台） |
| `/privacy/` | プライバシーポリシー（叩き台） |
| `/healthz` | 死活監視 |
| `/static/site.css` | サイトのスタイル |

LP の「借りられる GPU」は、提供中ノードのスナップショットから
GPU 名・ベンダー・VRAM・提供者表示名・空き状態だけを表示します（IP や内部 ID は出しません）。

**当面は無料**です。料金ページにもその旨を明記しています。

## 提供 PC

Docker Engine と GPU ドライバーを準備し、カタログのイメージをビルドします。

```bash
chmod 600 node-provider.secret.json
target/release/gpupod-agent devices
target/release/gpupod-tools doctor
target/release/gpupod-agent resources --memory-gib 8 --disk-gib 100 \
  --default-memory-gib 2 --default-disk-gib 25
target/release/gpupod-agent on
target/release/gpupod-agent daemon --config node-provider.secret.json
```

中央パネル側でも受付 ON にします。提供 PC は中央 HTTPS API をポーリングして操作を受け取り、
必要な間だけ中央 SSH へ逆向きのポート転送を作ります。提供 PC 側に公開ポートは不要です。

GPU をダミーにする場合は --mock-gpu をサブコマンドより前に付けてください。

```bash
target/release/gpupod-agent status
target/release/gpupod-agent off              # drain（既定）
target/release/gpupod-agent off --mode stop
target/release/gpupod-agent stats
target/release/gpupod-agent reconcile
```

提供 PC の状態は .provider-state/ の SQLite に保存します。

## 接続方式とポート

- **中央中継**: 中央の 19000/TCP を提供 PC の SSH 制御に使用します。19001〜19999/TCP の空きから、
  貸出ごとに最大 4 ポートをランダムに選び、DB で重複を防ぎます。SSH は利用者の公開鍵で認証します。
- **直接接続**: 対応する提供 PC では、利用者が直接接続を選べます。提供 PC の登録設定へ
  examples/direct-options.json の項目を追加します。

中継サービスは、その提供 PC に割り当てた実行中貸出のポートだけを許可します。
SSH シェルや任意宛先への接続は受け付けません。ポート範囲や中継制御ポートは
GPUPOD_RELAY_PORT[_MIN|_MAX] で変更できます。

## ディスク・RAM の事前予約

提供 PC 全体の貸出枠を先に設定し、その枠から貸出ごとに予約します。

```bash
target/release/gpupod-agent resources --memory-gib 8 --disk-gib 100 \
  --default-memory-gib 2 --default-disk-gib 25
```

設定・変更は受付 OFF かつ全貸出の返却完了後に行います。

## 補助ツール

```bash
target/release/gpupod-tools --help
```

doctor / systemd-units / auto-lend / rotate-token / prepare-mirrors / prepare-template /
prepare-runtime / support-bundle / benchmark / build-images / demo / release-repository /
update-agent / gpu-diagnostics / collective-probe を提供します。
ホストへのパッケージ導入やサービス有効化は行いません（unit ファイルの出力のみ）。

## gVisor

```bash
target/release/gpupod-tools prepare-runtime --output-dir /opt/gpupod/runtime-bundles/gvisor \
  --docker-config /etc/docker/daemon.json --gpu
```

x86_64 用の固定リリースを公式配布元から取得し、固定した SHA-512 を検証します。

## 署名付きの段階更新

Python 版の TUF リポジトリとは互換性がありません。ed25519 署名 + SHA-256 の
root / targets / snapshot / timestamp メタデータを生成・検証する簡易実装です。

```bash
target/release/gpupod-tools release-repository --repository release-repository \
  --keys-dir .release-keys init
target/release/gpupod-tools release-repository --repository release-repository \
  --keys-dir .release-keys publish --version 0.2.0 --sequence 1
target/release/gpupod-tools update-agent --config /etc/gpupod/update-options.json
```

## Python 版との主な差分

- Django admin は簡易管理画面（ユーザー作成・テンプレート有効化・更新設定）に置き換え。
- 署名付き更新は TUF ライブラリの代わりに ed25519 + SHA-256 の簡易実装（リポジトリ互換なし）。
- torch_smoke.py はコンテナ内スクリプトとして images/ に残す（Rust 化不要）。
- gunicorn の代わりに axum の単一バイナリで HTTP と SSH 中継を同居させる構成も可能。
- examples/、images/、docs/ の内容は Python 版と共通（Dockerfile・nginx 設定・テンプレートカタログ）。

## 検証

```bash
cargo build
cargo test
```

## クイックスタート（Docker）

Linux と Docker（Compose v2）があれば、中央サーバーと提供エージェント（GPU はダミー）を
1 コマンドで起動できます。初回はイメージのビルドに数分かかります。

```bash
git clone https://github.com/jimoto-no-llm/jimoto-no-gpupod-rs.git
cd jimoto-no-gpupod-rs

# Docker ソケットのグループ ID を自動設定して起動
./scripts/compose.sh up -d

# 管理者ユーザーも同時に作る場合（デモ用のローカルパスワード）
GPUPOD_SUPERUSER_PASSWORD=change-me ./scripts/compose.sh up -d
```

ブラウザーで <http://localhost:8000/> を開きます。これは**ローカル検証用**の構成です。

- GPU はダミー（mock）で、Docker・SSH・中継・返却は実際に動きます。
- 貸出を試すには、ホスト側でテンプレートイメージを用意します
  （`cargo build --release && target/release/gpupod-tools build-images`、またはレジストリから取得）。
- 状態は Docker ボリューム `gpupod_gpupod-state` に保存します。`./scripts/compose.sh down -v` で消えます。
- HTTP・Docker ソケットのマウント・ホストネットワークを使うため、インターネットへ公開しないでください。
  本番は [docs/deploy.md](docs/deploy.md) の VPS 構成を使ってください。

本番相当の構成（VPS 1 台 + systemd + nginx + TLS + ポート 19000〜19999）は
[docs/deploy.md](docs/deploy.md) にまとめています。

## イベントで使う

イベントでは「中央 1 台 + 提供 PC 数台」で運用します。提供 PC は外向き接続だけで参加でき、
公開ポートは不要です。

1. **中央を立てる**: [docs/deploy.md](docs/deploy.md)（VPS 1 台、TLS、19000〜19999/TCP の開放）。
2. **提供 PC を登録する**: 管理パネルの「提供PC管理」で登録し、`node-*.secret.json` を提供者へ渡す。
3. **提供 PC を準備する**: Docker と GPU ドライバーを入れ、テンプレートイメージをビルドする。

   ```bash
   target/release/gpupod-tools doctor
   target/release/gpupod-tools build-images --pytorch cuda   # GPU に合わせて cpu / rocm / xpu
   target/release/gpupod-agent resources --memory-gib 8 --disk-gib 100 \
     --default-memory-gib 2 --default-disk-gib 25
   target/release/gpupod-agent on
   target/release/gpupod-agent daemon --config node-provider.secret.json
   ```

4. **受付を ON にする**: 提供 PC 側を ON にしたあと、中央パネルでも受付 ON にする。
5. **参加者へ案内する**: URL、Discord ログイン、SSH 公開鍵、接続方法（`ssh -p <割当ポート>`）。

当日の受付 ON/OFF、ピーク時の調整、片付け、障害時の確認手順は
[docs/operations.md](docs/operations.md) にまとめています。
セキュリティ上の前提と制約は [docs/security.md](docs/security.md) を参照してください。

## ドキュメント

| ドキュメント | 内容 |
| --- | --- |
| [docs/usage.md](docs/usage.md) | 利用者・提供者向けの詳しい手順 |
| [docs/deploy.md](docs/deploy.md) | 本番の VPS 構成（systemd / nginx / TLS / ポート） |
| [docs/operations.md](docs/operations.md) | イベント当日の運用手順・障害時の確認 |
| [docs/security.md](docs/security.md) | 脅威モデル・既知の制約・依存の脆弱性 |
| [docs/development.md](docs/development.md) | 開発環境・CI・コードの決まり |
| [docs/roadmap.md](docs/roadmap.md) | ロードマップと設計判断 |
| [CONTRIBUTING.md](CONTRIBUTING.md) | 開発への参加方法・PR の作法 |
| [SECURITY.md](SECURITY.md) | 脆弱性の報告先（非公開） |

## 開発への参加

Issue / PR を歓迎します。まず [CONTRIBUTING.md](CONTRIBUTING.md) を読んでください。

```bash
./scripts/check.sh     # fmt / clippy / test
```

CI（GitHub Actions）は PR ごとに fmt・clippy・build・test を実行し、
依存の脆弱性は PR と週次で `cargo audit` により確認します。
失敗時のログはワークフローのアーティファクトに保存されます。
