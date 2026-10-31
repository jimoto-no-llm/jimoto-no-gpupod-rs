# イベント運用手順

イベント当日に「みんなが RunPod のように GPU を借りられる」状態を保つための手順です。
構成の作り方は [deploy.md](deploy.md)、設計上の制約は [security.md](security.md) を参照してください。

> 前提: 中央は systemd の `gpupod-control.service`、提供 PC は `gpupod-provider.service` で常駐。
> コマンド例の `gpu.example.org` は自分のドメインに置き換えてください。

## 役割

| 役割 | 担当すること |
| --- | --- |
| 中央管理者 | 受付 ON/OFF、提供 PC の状態監視、利用者対応、障害の切り分け |
| 提供者 | 自分の PC の受付 ON/OFF、資源枠の設定、GPU・Docker の状態確認 |
| 受付（フロント） | 参加者への案内、SSH 鍵の準備、トラブル一次対応 |

1 人で全部を兼ねる場合でも、**受付を止める判断だけは迷わずできるようにしておきます。**

## 1 週間前まで

1. 中央サーバーを [deploy.md](deploy.md) の手順で構築し、TLS と Discord ログインを確認する。
2. 提供 PC を登録し、`node-*.secret.json` を提供者へ安全に渡す（チャット添付は避け、直接渡すか期限付きリンク）。
3. 提供 PC ごとに資源枠を設定する。
4. 参加予定人数と GPU の空きから、同時貸出数の目安を決める（1 貸出 = 中継 1〜4 ポート）。
5. リハーサル: 実際に 1 件借りて SSH 接続・ファイル転送・返却まで通す。

```bash
# 提供 PC 側
chmod 600 node-provider.secret.json
gpupod-agent devices                       # GPU が見えるか
gpupod-tools doctor                        # Docker と環境の診断
gpupod-agent resources --memory-gib 8 --disk-gib 100 \
  --default-memory-gib 2 --default-disk-gib 25
gpupod-agent on
gpupod-agent daemon --config node-provider.secret.json
```

## 前日

- [ ] `curl -fsS https://gpu.example.org/healthz` が `{"status":"ok"}`
- [ ] 管理者でログインでき、提供 PC がオンライン表示
- [ ] テンプレートが中央と提供 PC で一致し、利用可能に見える
- [ ] 空き GPU・資源枠が想定どおり
- [ ] 中央の状態バックアップを取得（`./scripts/backup-control-state.sh`）
- [ ] 運営用アカウントと連絡手段（Discord など）を確認
- [ ] 提供者へ「当日の受付 ON 時刻」と「止め方」を共有

```bash
# 中央
./scripts/health-check.sh https://gpu.example.org
sudo -u gpupod -H sh -c 'set -a; . /etc/gpupod/control.env; set +a; \
  sqlite3 "$GPUPOD_CONTROL_STATE/control.sqlite3" \
  "select name, enabled, desired_enabled, last_seen from portal_node;"'
```

## 当日の流れ

### 開始 30 分前

1. 中央のヘルスチェックと中継ポートの待ち受けを確認する。
2. 提供 PC の在庫・受付状態を確認する（`gpupod-agent status` / `stats`）。
3. 利用者向け案内（URL、ログイン方法、SSH 鍵の作り方）を掲示する。

```bash
# 中央
curl -fsS https://gpu.example.org/healthz
ss -lntp | grep -E ':(19000)\b'
journalctl -u gpupod-control.service --since '10 min ago' --no-pager | tail -30

# 提供 PC
gpupod-agent --state-dir /var/lib/gpupod/provider status
gpupod-agent --state-dir /var/lib/gpupod/provider stats
```

### 受付を開始する

**おすすめの順番**: 先に提供 PC 側を ON にし、そのあと中央パネルで受付 ON にします。

```bash
# 提供 PC（手動で ON にする場合）
gpupod-agent --state-dir /var/lib/gpupod/provider on
```

中央パネルの「提供 PC 管理」からも ON / drain / stop を要求できます。
自動で混雑を検知して受付を制御する場合は、提供 PC で次を常駐させます。

```bash
gpupod-tools auto-lend --state-dir /var/lib/gpupod/provider --mode drain --idle-seconds 60
```

### ピーク時

- パネルの空き状況と、提供 PC の `stats` を見て、資源の残りを確認します。
- 起動待ちや後片付け中の貸出も資源を消費します。返却が進まない場合は次を確認します。
  1. 利用者に成果物のコピーと返却を案内済みか
  2. 提供 PC がオンラインか（`last_seen` が新しいか）
  3. 提供 PC 側で `gpupod-agent reconcile` を実行する
- 受付を絞るときは、まず新規受付だけ止めます（実行中の貸出は継続）。

```bash
# 新規受付のみ停止（drain）
gpupod-agent --state-dir /var/lib/gpupod/provider off

# 受付も実行も止めて片付ける（stop）— 実行中の作業が消えるため、合意してから
gpupod-agent --state-dir /var/lib/gpupod/provider off --mode stop
```

### 終了 30 分前

1. 新規受付を停止する（中央パネル + 提供 PC）。
2. 利用中の貸出に「まもなく終了」をアナウンスし、成果物のコピーを促す。
3. 返却が完了したことをパネルで確認する。

### 片付け

1. すべての貸出が返却済みかを確認する。
2. 提供 PC のコンテナと作業ボリュームが残っていないか確認する。
3. 受付を OFF にし、必要ならデーモンを停止する。
4. 中央の状態をバックアップし、ログを保存する。

```bash
# 中央: 受付要求を止めたうえで、実行中の貸出が残っていないか確認
sudo -u gpupod -H sh -c 'set -a; . /etc/gpupod/control.env; set +a; \
  sqlite3 "$GPUPOD_CONTROL_STATE/control.sqlite3" \
  "select status, count(*) from portal_lease where active group by status;"'

# 提供 PC: 状態と実態を突き合わせ、残骸を確認
gpupod-agent --state-dir /var/lib/gpupod/provider reconcile
docker ps -a --filter name=gpupod --format '{{.Names}} {{.Status}}'
docker volume ls --filter name=gpupod

# バックアップとログ保存
./scripts/backup-control-state.sh
sudo journalctl -u gpupod-control.service --since today --no-pager > control-$(date +%F).log
```

`off` は「新規受付を止める」だけで、利用中のコンテナやメモリを強制的に回収しません。
返却が済むと資源が戻ります。終了後にコンテナが残っている場合は、利用者への確認のうえ
`off --mode stop` を使うか、提供者が `docker ps` で対象を確認して削除します。

## 障害時の確認

上から順に確認すると切り分けが早いです。

### 1. 中央が応答しない

```bash
curl -fsS https://gpu.example.org/healthz
sudo systemctl status gpupod-control.service
sudo journalctl -u gpupod-control.service -n 100 --no-pager
sudo nginx -t && sudo systemctl status nginx
sudo ss -lntp | grep -E ':(8000|19000)\b'
df -h /var/lib/gpupod
```

- サービスが落ちている: `sudo systemctl restart gpupod-control`。再起動で中継も切れるため、
  貸出中の利用者へ一声かけてから行います。
- ディスク不足: バックアップの古い分を削除し、`gpupod-control migrate` の前に空きを作ります。
- DB がロックしている: 管理画面の操作を止め、`sqlite3` の書き込みをやめてから再起動します。

### 2. 提供 PC がオンラインにならない

```bash
# 提供 PC
sudo systemctl status gpupod-provider.service
sudo journalctl -u gpupod-provider.service -n 100 --no-pager
curl -fsS https://gpu.example.org/healthz         # 外向き HTTPS が通るか
gpupod-agent --state-dir /var/lib/gpupod/provider status
docker ps >/dev/null && echo docker-ok
```

- 401/403 が続く: 機材キー（`node-*.secret.json`）が一致していません。中央で再登録し、
  新しいファイルを配置してください（古いファイルは削除）。
- 時計が大きくずれている: ハートビートが失効します。`timedatectl` で NTP を確認。
- GPU が見えない: `gpupod-agent devices` と `gpupod-tools doctor` を実行し、
  ドライバーと Docker の状態を確認します。

### 3. 借りられない／起動に失敗する

1. 中央と提供 PC の両方で受付 ON か。
2. 中央でテンプレートが有効か。提供 PC のカタログ指紋と一致しているか。
3. 資源枠と予約の合計（`stats`）に空きがあるか。
4. 提供 PC にイメージがあるか（`docker images`）。
5. パネルのエラーメッセージ、`journalctl -u gpupod-provider` を確認。

### 4. SSH だけつながらない

1. 貸出の状態が `running` か（起動中はまだ接続できません）。
2. パネルに表示された接続先ポートとホスト名を使っているか。
3. 中央の 19000〜19999/TCP が開いているか、提供 PC が外向き接続できているか。
4. 利用者が正しい秘密鍵を使い、公開鍵を貸出時に登録したか。
5. 直接接続を選んだ場合: 提供 PC の到達性、DNS、NAT、`direct_host` / `direct_bind`。

### 5. 返却が完了しない

- 提供 PC がオフラインだと後片付けが進まず、資源は予約されたままです。
- 提供 PC が復帰したら `gpupod-agent reconcile` を実行します。
- それでも戻らない場合は、提供者が `docker ps` / `docker volume ls` で対象を確認してから
  `gpupod-agent status` の表示と突き合わせます。

調査用の情報は `gpupod-tools support-bundle` で伏字済みアーカイブにできます（自動送信はしません）。

```bash
gpupod-tools support-bundle --config node-provider.secret.json
```

## エスカレーション

- セキュリティ疑い（不正アクセス、情報漏えい、認証回避）: 公開 Issue に書かず [SECURITY.md](../SECURITY.md) へ。
- 提供 PC の障害: 提供者へ連絡し、`support-bundle` の取得を依頼。
- 中央の障害で復旧できない: バックアップから復元（[deploy.md](deploy.md) の「運用」）。

## バックアップ（cron 例）

```cron
# 毎日 04:00 に中央の状態をバックアップ（保持は 14 日）
0 4 * * * cd /opt/gpupod && ./scripts/backup-control-state.sh --keep 14 >> /var/log/gpupod-backup.log 2>&1
```

バックアップには `relay-host-key` と DB が含まれます。**秘密情報として扱い**、
リポジトリや公開ストレージへ置かないでください。

## やってはいけないこと

- イベント中に中央を安易に再起動する（中継が切れます）
- `control.env` や `node-*.secret.json` をチャットや Issue に貼る
- 提供 PC の受付を、利用者の作業が残ったまま `off --mode stop` にする
- 貸出中のポートを手動で塞いだり、中央の DB を直接書き換えたりする
