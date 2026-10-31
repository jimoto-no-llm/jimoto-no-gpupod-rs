# 運用スクリプト

イベント運用・デプロイ・検証で使う補助スクリプトです。
Rust のバイナリ（`gpupod-control` / `gpupod-agent` / `gpupod-tools`）を前提にしています。

| スクリプト | 用途 | 実行場所 |
| --- | --- | --- |
| `compose.sh` | `docker compose` を `DOCKER_GID` 付きで実行（デモ起動） | 開発 PC |
| `check.sh` | fmt / clippy / test をまとめて実行 | 開発 PC |
| `health-check.sh` | 中央の `/healthz` と中継ポートの確認 | 中央サーバー・監視 |
| `backup-control-state.sh` | DB・中継ホスト鍵・設定のバックアップ | 中央サーバー |
| `install-systemd.sh` | systemd ユニットの配置 | 中央サーバー・提供 PC |
| `docker/` | コンテナのエントリポイント（Dockerfile から使用） | イメージ内 |
| `systemd/` | systemd ユニット（`install-systemd.sh` が配置） | 中央サーバー・提供 PC |

## 例

```bash
# ローカルの検証
./scripts/check.sh

# デモ環境（中央 + mock 提供エージェント）
./scripts/compose.sh up -d
./scripts/compose.sh logs -f control

# 中央サーバーでの日常運用
./scripts/health-check.sh https://gpu.example.org gpu.example.org 19000
sudo ./scripts/backup-control-state.sh --keep 14
```

- `compose.sh` は Docker ソケットのグループ ID を自動設定します。`docker compose` を直接使う場合は
  `DOCKER_GID` を自分で設定してください。
- バックアップは秘密情報を含みます（0600）。リポジトリや公開ストレージへ置かないでください。
- 詳細な手順は [docs/deploy.md](../docs/deploy.md) と [docs/operations.md](../docs/operations.md) を参照してください。
