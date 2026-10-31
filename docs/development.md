# 開発ガイド

このリポジトリで開発するときの環境、コマンド、CI、コードの読み方をまとめます。
利用者・提供者向けの手順は [usage.md](usage.md)、本番構成は [deploy.md](deploy.md) を参照してください。

## 1. 必要なもの

- Rust 1.85 以上（`rust-version`）。CI は最新 stable で実行します。
- Linux（WSL2 可）。貸出コンテナを動かす場合のみ Docker Engine。
- 任意: `fio`（測定）、`shellcheck`（スクリプト）、`sqlite3`（状態確認）。

GPU は必須ではありません。`--mock-gpu` とデモ構成で一通り確認できます。

## 2. よく使うコマンド

```bash
cargo build --workspace                  # 3 バイナリ
cargo test --workspace                   # テスト
cargo clippy --workspace --all-targets   # 静的検査
cargo fmt --all                          # 整形
./scripts/check.sh                       # 上記をまとめて実行
```

CI と同じ条件で確認したい場合:

```bash
cargo build --workspace --all-targets --locked
cargo test --workspace --locked --no-fail-fast
cargo audit                              # RustSec
```

## 3. GPU なしで動かす

ローカルで中央 + 提供エージェントを起動する方法は 2 つあります。

1. Docker（推奨・ビルド済みバイナリが不要）

```bash
./scripts/compose.sh up -d
# http://localhost:8000/ （GPUPOD_SUPERUSER_PASSWORD を設定した場合のみ管理者作成）
./scripts/compose.sh logs -f control
./scripts/compose.sh down
```

2. バイナリを直接起動

```bash
cargo build --release
target/release/gpupod-control migrate
target/release/gpupod-control init-control
target/release/gpupod-control sync-templates examples/templates.json
target/release/gpupod-control create-superuser
target/release/gpupod-control serve --bind 127.0.0.1:8000
```

別プロセスで提供エージェント（`--mock-gpu`）:

```bash
target/release/gpupod-agent --mock-gpu --state-dir .provider-state \
  --templates examples/templates.json on
target/release/gpupod-agent --mock-gpu --state-dir .provider-state \
  --templates examples/templates.json daemon --config node-provider.secret.json
```

HTTP で試すときは `GPUPOD_INSECURE_HTTP=1` を付けます。状態は `.control-state/` と
`.provider-state/` に保存され、どちらも git 管理外です。

## 4. リポジトリ構成

| パス | 内容 |
| --- | --- |
| `crates/gpupod-common` | 共通型・テンプレート指紋・資源計算・同期プロトコル |
| `crates/gpupod-control` | WebUI / API / SSH 中継 / 認証 / DB（axum + rusqlite + russh） |
| `crates/gpupod-agent` | 提供 PC の常駐エージェント（Docker 操作は bollard） |
| `crates/gpupod-tools` | 補助ツール（doctor / systemd-units / build-images / support-bundle など） |
| `examples/` | テンプレートカタログ、env、nginx の設定例 |
| `images/` | 貸出イメージ（Ubuntu / PyTorch）の Dockerfile |
| `docs/` | 使い方・デプロイ・運用・セキュリティ・開発 |
| `scripts/` | 運用スクリプトと systemd ユニット |
| `.github/` | CI、Issue / PR テンプレート、CODEOWNERS、Dependabot |

## 5. コードの決まり

- **Python/Django 版との互換性を優先**します。テンプレートカタログの正規化 JSON と指紋、
  同期プロトコル、エラーメッセージの互換性を変える場合は、PR に影響を明記してください。
- ルートの `Cargo.toml` はメンテナが管理します。クレート追加は PR で提案し、
  各 crate では `.workspace = true` を参照します。
- 利用者から任意のイメージ・ホストマウント・Docker オプションを受け付けない方針を維持します。
- 秘密情報（トークン、鍵、Cookie、ホストパス）はログ・エラー・Issue に出さない。
- フォーマットは `rustfmt.toml`、lint は `clippy.toml`（MSRV 1.85）に従います。
- DB スキーマの変更は `crates/gpupod-control/src/db.rs` のマイグレーションに追加し、
  既存 DB からの移行を必ずテストします。

## 6. CI

| ワークフロー | 実行タイミング | 内容 |
| --- | --- | --- |
| `.github/workflows/ci.yml` | main への push / PR / 手動 | fmt（非ブロック）、clippy、build、test。失敗時はログをアーティファクト保存 |
| `.github/workflows/audit.yml` | Cargo 関連の PR / 週次（月曜 00:00 UTC）/ 手動 | `cargo audit`（RustSec） |
| `.github/workflows/docker.yml` | Dockerfile など関連ファイルの PR / main | `docker compose config`、`shellcheck`、イメージビルド |

依存キャッシュは `Swatinem/rust-cache` を使います。各ジョブは独立して並列に走ります。

### 現在のベースライン（2026-10-31）

CI を緑に保ちながら、段階的に厳しくしていくための現状記録です。

| 項目 | 現状 | 目標 |
| --- | --- | --- |
| `cargo fmt --all --check` | 71 ファイル・576 箇所に差分（**非ブロック**） | `cargo fmt --all` を適用してブロック化 |
| `cargo clippy --workspace --all-targets` | 警告あり（error なし）。`-D warnings` は未設定 | 警告を整理して `-D warnings` を有効化 |
| `cargo test --workspace` | 成功（200 テスト） | 維持 |
| `--all-features`（full-stack） | **コンパイルエラー**（`gpupod-tools/src/demo.rs` が `Config` の新フィールド未対応） | 修正後に CI へ追加 |
| `cargo audit` | 既知 4 件 + 未メンテ 1 件を `.cargo/audit.toml` で除外（[security.md](security.md) 参照） | russh 系の更新で除外を削除 |

整形を適用する PR は Rust ソースを広く変更するため、メンテナがまとめて実施してください。
適用後は `.github/workflows/ci.yml` の fmt ジョブから `continue-on-error` を外します。

### 既知の失敗（2026-10-31 の整備時点・要修正）

いずれもリポジトリ整備の範囲外（Rust ソース・テスト）のため、CI 整備では修正していません。
修正済みの項目はこの節から削除してください。

1. `crates/gpupod-tools/src/demo.rs` が `gpupod_control::Config` の Discord 関連フィールド
   （`discord_client_id` など 6 個）を初期化しておらず、`--features full-stack` がコンパイルできません。
   （そのため CI は既定フィーチャーで build / test しています。）
2. 既存 Rust ソース全体が rustfmt 未適用です（`cargo fmt --all` で一括整形できます）。

## 7. テストの考え方

- 各 crate は `tests/` に統合テストを持ちます。ネットワーク・Docker はモックで差し替えます。
- テンプレートや資源計算など Python 版と互換が必要な処理は、固定値との一致をテストします。
- 貸出の状態遷移（要求 → 起動 → 返却 → 後片付け）は失敗系も含めてテストします。
- 実機（Docker / GPU）が必要な確認は、PR の「検証」に実施内容と未検証範囲を書きます。

## 8. デバッグ

```bash
RUST_LOG=debug target/release/gpupod-control serve --bind 127.0.0.1:8000
RUST_LOG=debug target/release/gpupod-agent --state-dir .provider-state status
sqlite3 .control-state/control.sqlite3 '.tables'
sqlite3 .control-state/control.sqlite3 'select name,enabled,desired_enabled,last_seen from portal_node;'
```

- 中央の状態は `GPUPOD_CONTROL_STATE`、提供 PC は `--state-dir` で変えられます。
- 設定は環境変数です。`examples/control.env` が一覧の雛形です。
- 問い合わせ用ログは `gpupod-tools support-bundle`（伏字済み・自動送信なし）。

## 9. GitHub リポジトリの設定（管理者作業）

CI とテンプレートはリポジトリに含まれていますが、次の設定は GitHub 上で有効化してください。
`.github/` を見ても分からないため、ここに残します。

- **Branch protection（main）**: PR 必須、直 push 禁止、必要なチェックに
  `fmt` / `clippy` / `build` / `test` / `cargo audit (RustSec)` を指定。
  レビューは CODEOWNERS に従って 1 名以上。
- **Security**: Private vulnerability reporting を有効化（`SECURITY.md` の導線）。
  Dependabot alerts / security updates を有効化（`.github/dependabot.yml` はバージョン更新の PR）。
  Secret scanning と push protection を有効化。
- **Actions**: 既定の権限を「読み取りのみ」にし、ワークフローからの書き込みは明示的に許可する。
  Fork PR からのワークフロー実行は承認制にする。
- **Discord OAuth**: Redirect URI に `https://<ドメイン>/oauth/discord/callback` を登録。
- **ラベル**: `bug` / `enhancement` / `dependencies` / `security` を作成しておくと Dependabot の分類が揃います。

## 10. リリース

1. `CHANGELOG.md` を更新する。
2. バージョンを上げる（ワークスペースの `version` と各 crate の参照）。
3. タグを打ち、リリースノートを書く。
4. 中央と提供 PC の更新手順は [deploy.md](deploy.md) の「運用」に従う。

署名付き更新のリポジトリ管理は `gpupod-tools release-repository` を使います。
