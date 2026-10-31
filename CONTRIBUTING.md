# コントリビューションガイド

Jimoto GPU Pod への関心ありがとうございます。イベントで配れる GPU 貸出サービスを一緒に育てましょう。
このドキュメントは開発環境の作り方、テスト、PR の作法をまとめたものです。

## まず読むもの

| ドキュメント | 内容 |
| --- | --- |
| [README](README.md) | 全体像、クイックスタート、各機能 |
| [docs/development.md](docs/development.md) | 開発環境の詳細、CI、コードの読み方 |
| [docs/deploy.md](docs/deploy.md) | 本番の VPS 構成（systemd / nginx / TLS） |
| [docs/security.md](docs/security.md) | 脅威モデルと既知の制約 |
| [SECURITY.md](SECURITY.md) | 脆弱性の報告先（非公開） |

## 開発環境

Linux を前提にしています（WSL2 でも可）。GPU は必須ではありません。

- Rust 1.85 以上（`rustup` 推奨）
- Docker Engine（貸出コンテナを実際に動かす場合）
- 任意: `fio`（回線・ディスク測定）、`shellcheck`（シェルスクリプト）

```bash
git clone https://github.com/jimoto-no-llm/jimoto-no-gpupod-rs.git
cd jimoto-no-gpupod-rs
cargo build --workspace
cargo test --workspace
```

GPU なしで一通り試す手順は [README](README.md) の「GPU がなくても試す」と
[docs/development.md](docs/development.md) を参照してください。Docker がある場合は
`./scripts/compose.sh up -d` で中央サーバー + mock 提供エージェントを起動できます。

## 変更を出す前に

1. 既存の Issue を確認し、大きめの変更は先に方針を共有する
2. `main` から作業ブランチを切る（例: `feat/lease-history`）
3. 変更とテストを同じ PR に含める（テストを追加できない場合は理由を書く）
4. ローカルで検証する

```bash
cargo fmt --all                  # 整形（Rust ソースを触る場合）
cargo clippy --workspace --all-targets
cargo test --workspace
./scripts/check.sh               # fmt / clippy / test をまとめて実行
```

5. 秘密情報をコミットしない（`node-*.secret.json`、`.control-state/`、`.env` は `.gitignore` 済み）

## PR の作法

- 日本語で、次の 4 行構成にします（テンプレートあり）。概要 / 変更 / 検証 / 注意点。
- 検証には実行したコマンドと結果を書きます。Docker や GPU の実機確認ができない場合は、
  「未検証」とその理由・影響範囲を明記してください。
- 破壊的な変更（DB スキーマ、設定キー、CLI、運用手順）は、移行方法とロールバックを「注意点」に書きます。
- レビューは [CODEOWNERS](.github/CODEOWNERS) に従います。指摘への対応は、修正内容を返信で要約してください。
- マージは squash を基本とし、履歴を読みやすく保ちます。

## リポジトリの決まり

- `crates/gpupod-*` は Python/Django 版との互換性を最優先します。テンプレートカタログの指紋や
  同期プロトコルを変える場合は、互換性への影響を PR に明記してください。
- ルートの `Cargo.toml` はメンテナが管理します。クレートの追加・更新が必要なときは PR で提案してください
  （各 crate の `Cargo.toml` では `.workspace = true` を参照します）。
- 利用者から任意のイメージ・ホストマウント・Docker オプションを受け付けない方針を維持してください。
- ログやエラーメッセージの秘密情報は、常に伏字にします。
- 運用手順を変えた場合は `docs/` も同じ PR で更新します。

## CI

PR では `.github/workflows/ci.yml` が fmt / clippy / build / test を、
`.github/workflows/audit.yml` が RustSec の依存監査を実行します。
現在のベースライン（未整形の箇所、clippy の警告数）は [docs/development.md](docs/development.md) にあります。

## コミュニティ

- 行動規範: [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md)
- イベント運営の相談: Issues（機能要望）または運営の Discord
- セキュリティ: [SECURITY.md](SECURITY.md)（公開 Issue に書かない）
