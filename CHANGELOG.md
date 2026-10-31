# Changelog

このプロジェクトの主な変更を記録します。形式は [Keep a Changelog](https://keepachangelog.com/ja/1.1.0/)、
バージョンは [Semantic Versioning](https://semver.org/lang/ja/) に従います。

## [Unreleased]

### Added

- CI（GitHub Actions）: `cargo fmt --check`、clippy、build、test をジョブ分割し、依存クレートのキャッシュを利用。
  失敗時はログをアーティファクトとして保存。
- 依存脆弱性チェック: `cargo audit` を PR と週次（月曜 00:00 UTC）で実行。
- Docker 実行環境: マルチステージ・非 root の `Dockerfile`、`.dockerignore`、
  `docker-compose.yml`（中央サーバー + mock GPU の提供エージェントを 1 コマンド起動）。
- 運用スクリプト: `scripts/compose.sh`、`scripts/check.sh`、`scripts/health-check.sh`、
  `scripts/backup-control-state.sh`、`scripts/install-systemd.sh`、systemd ユニット。
- ドキュメント: `docs/deploy.md`（VPS 1 台 + systemd + nginx + TLS）、`docs/operations.md`（イベント当日の運用手順）、
  `docs/security.md`（脅威モデル・既知の制約）、`docs/development.md`（開発環境・CI）。
- リポジトリ運用: Issue テンプレート（バグ / 機能要望）、PR テンプレート、CODEOWNERS、Dependabot（cargo / GitHub Actions / Docker）、
  `SECURITY.md`、`CONTRIBUTING.md`、`CODE_OF_CONDUCT.md`、`.editorconfig`、`rustfmt.toml`、`clippy.toml`。
- README: Docker クイックスタートとイベントでの使い方を追記。

### Changed

- CI は従来の `verify.yml` を `ci.yml` に統合（build / test に加えて fmt / clippy を追加）。

### Fixed

- README 先頭の余分なコードフェンスを削除（本文が正しく表示されない問題）。

### Known issues（2026-10-31 の整備時点。修正済みなら削除してください）

- 既存 Rust ソースの多くが rustfmt 未適用です。整形PRが入るまで CI の fmt は非ブロック（警告）です。
- 整備時点では `gpupod-tools` の `--features full-stack`（demo）が `Config` の新フィールド未対応で
  コンパイルできませんでした。CI は既定フィーチャーで実行しています。
- clippy の警告が残っているため、CI はまだ `-D warnings` を有効にしていません。
- 依存に RustSec 勧告があります（russh 0.54.5 ほか）。詳細と対応は `docs/security.md` を参照してください。

## [0.1.0] - 2026-10-29

### Added

- Python/Django 版からの Rust 移植（gpupod-common / gpupod-control / gpupod-agent / gpupod-tools）。
- 中央サーバーの WebUI・API・SSH 中継、Discord OAuth ログイン、公開 LP。
- 提供PCエージェント（Docker 操作、状態管理、署名付き更新の検証）。
