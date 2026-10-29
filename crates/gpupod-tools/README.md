# gpupod-tools

Jimoto GPU Pod（Rust 版）の補助ツール。単一バイナリ `gpupod-tools` に
clap のサブコマンドとして実装している。Python 版 `helpers/` の移植。

## サブコマンド

| コマンド | Python 原本 | 内容 |
| --- | --- | --- |
| `doctor` | doctor.py | GPU 品目・指標・Docker 情報の読取専用診断 |
| `systemd-units` | systemd_units.py | unit 文面の生成（インストールは操作者） |
| `auto-lend` | auto_lend.py | GPU 活動検出と自動貸出制御 |
| `rotate-token` | rotate_token.py | 機材キーの 2 段階ローテーション |
| `prepare-mirrors` | prepare_mirrors.py | daemon.json 候補の生成 |
| `prepare-template` | prepare_template.py | 承認済み画像の pull（bollard） |
| `prepare-runtime` | prepare_runtime.py | gVisor バンドル取得と daemon.json 生成 |
| `support-bundle` | support_bundle.py | 伏字済みサポートアーカイブ |
| `benchmark` | benchmark.py | fio / 中央往復の測定 |
| `build-images` | build_images.py | ローカル画像の docker build |
| `demo` | demo.py | 中央＋提供の同居デモ（full-stack feature） |
| `release-repository` | release_repository.py | TUF 風リポジトリの init / publish / refresh |
| `update-agent` | update_agent.py | 署名検証つき更新とロールバック |
| `gpu-diagnostics` | gpu_diagnostics.py | GPU 予約つき all-reduce 検査 |
| `collective-probe` | collective_probe.py | コンテナ内プローブの呼び出し仕様 |

## ビルドとテスト

```sh
cargo build -p gpupod-tools
cargo test -p gpupod-tools
# 中央＋提供の同居デモを有効化する場合
cargo build -p gpupod-tools --features full-stack
```

ユニットテストは 96 件（quote/generate の文面一致、mirror 検証、伏字処理、
測定スキーマ、TUF 風署名の往復と改ざん検出、安全な展開、ローテーション再開など）。

## gpupod-agent / gpupod-control との関係

実装開始時点では gpupod-agent / gpupod-control に lib 実装がなかったため、
本 crate は補助ツールが使う最小限を `src/support/` に重複実装している。
後から lib 実装が揃ったため、`--features full-stack` で gpupod-agent /
gpupod-control に path 依存し（デモ用）、残りの重複は削除予定。
既定ビルドは両 crate を含まないので、他 crate の変更に影響されない。

重複している範囲:

| gpupod-tools | gpupod-agent 側の同等物 |
| --- | --- |
| `support/state.rs` | `StateStore`（sqlite の agent_state テーブル。Python 版と同一スキーマ） |
| `support/agentlite.rs` | `Agent`（status / set_auto_busy / 診断レースの開始と解消） |
| `support/docker.rs` | `DockerEngine`（info / pull / create / exec / cleanup） |
| `support/devices.rs` | `load_gpus` / `discover_gpus` / `mock_gpus` |
| `support/telemetry.rs` | `collect` / `IdlePolicy` / `external_gpu_activity` |
| `support/config.rs` | `load_config`（daemon.load_config の移植） |
| `support/files.rs` | `atomic_json` / `atomic_bytes` / ファイルロック |
| `support/benchmarks.rs` | 結果スキーマ（public_results / save_result） |
| `support/sshkey.rs` | SSH 公開鍵の検証と生成（asyncssh 相当） |
| `support/tuf.rs` | TUF 風メタデータの署名と検証（update-agent が使用） |

## Python 版との差分

- **systemd-units**: unit の構造・引用符付け・文面は Python 版とバイト一致するが、
  `ExecStart` は現状 Python レイアウト（`.venv/bin/python` ＋ `helpers/*.py`）のまま。
  Rust バイナリ前提への切替はエントリパス定数の差し替えで対応できる。
- **demo**: 中央（gpupod-control の web + relay）と提供（gpupod-agent の ProviderDaemon、
  ダミー GPU）を同一プロセスで起動する。`--features full-stack` で有効化される。
  既定ビルドでは画像ビルドと状態ディレクトリ準備の後にエラーで停止する。
  管理者アカウント作成は `gpupod-control create-superuser` に任せる
  （Python 版は Django のコマンドでデモ用ユーザーを直接作っていた）。
- **release-repository**: Python 版の `tuf` ライブラリとは**互換がない**。
  署名鍵は ed25519-dalek（PKCS#8 PEM）、署名対象は "signed" 節の決定的 JSON、
  root / targets / snapshot / timestamp の 4 ロール構成という簡易実装。
  配布対象は Rust リポジトリ（`crates/ examples/ images/ docs/` とルート直下の定義ファイル）。
  アーカイブ内のリリース識別は `release.json`（Python 版は `provider_agent/release.json`）。
- **update-agent**: 同じく自前の TUF 風メタデータを検証する。
  Python 版の venv 作成と pip install は行わない（ソース配布のため）。
  アンパック・識別照合・リンク切替・systemctl restart・ヘルス確認・ロールバックは移植。
- **support-bundle**: `dependencies` は Python パッケージ版ではなく
  gpupod-tools のバージョンと `docker --version` を記録する。
  `python` キーは `tools_version` に変わった。
  状態 sqlite の読み取りは同一スキーマ（agent_state）を直接読む。
- **gpu-diagnostics**: SSH トンネルや公開ポートは張らず、診断専用コンテナを
  作成 → 起動 → コンテナ内 Python（collective-probe.py）を exec → 解除する。
  ラベル所有（io.gpulend.lease / io.gpulend.node）の確認と予約の保持は Python 版と同じ。
- **collective-probe**: 実 Torch 実行はコンテナ内の Python スクリプトが担う。
  ホスト側は backend / workers を検証し、コンテナ内コマンド仕様を返す。
- **benchmark**: ディスク測定は fio 3.x の direct I/O 1MiB QD1 を子プロセス実行する点まで同じ。
  ネットワーク測定は中央 `/api/nodes/<id>/benchmark/` への上下転送（1〜64MiB）。
- **doctor**: 指標の取得元は psutil ではなく `/proc` と `statvfs`。
  nvidia-smi の利用可否と DRM sysfs の読み取りは Python 版と同じ。

## 制約

- `gpupod-common` は変更していない（`GPU` / `Template` / `load_templates` /
  `resources` / `identifier` / `endpoint_host` を利用）。
- パッケージの追加は `fancy-regex`（support-bundle の lookbehind 付き正規表現移植）と
  `futures-util`（bollard のストリーム読み取り）のみ。どちらもルートの workspace 未登録。
