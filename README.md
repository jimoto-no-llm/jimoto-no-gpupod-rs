```
# Jimoto GPU Pod (Rust)

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

中央は axum + SQLite（rusqlite）+ サーバーサイドセッション、中継は russh（pure Rust SSH）、
提供側のコンテナ操作は bollard（Docker Engine API）、状態保存は SQLite + flock、
ホスト統計は sysinfo + /proc、署名付き更新は ed25519 + SHA-256 の簡易 TUF 風実装を使用します。

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