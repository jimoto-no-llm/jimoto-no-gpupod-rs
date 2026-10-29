# Jimoto GPU Pod の使い方

中央サーバーと提供 PC は別の役割です。まず GPU なしのデモで操作を確認し、その後に中央サーバー、提供 PC の順に用意します。以下のコマンドは、特に指定がなければリポジトリのディレクトリで実行します。

| 役割 | 用意するもの | 普段の操作 |
| --- | --- | --- |
| 中央管理者 | Linux、Rust バイナリ 1 つ、HTTPS、公開ポート | アカウント・提供 PC・テンプレートの管理 |
| 提供者 | Linux、Docker Engine、対応 GPU 環境 | 提供する資源の設定、受付 ON/OFF、診断 |
| 利用者 | Web ブラウザー、SSH クライアント、SSH 鍵 | テンプレートを選んで借りる、SSH 接続、返却 |

## 1. GPU なしで一通り試す

Linux 上で Docker Engine が起動し、現在のユーザーが Docker ソケットを操作できることを確認します。Rust のビルド環境（cargo）も必要です。

```bash
git clone <このリポジトリ>
cd jimoto-no-gpupod-rs
cargo build --release
target/release/gpupod-tools demo --pytorch
```

初回は Ubuntu 24.04 / 26.04 と CPU PyTorch のイメージをビルドします。起動したら `http://127.0.0.1:8000/` を開き、ターミナルに表示されたパスワードでログインします。

- 管理者: `demo-admin`
- 利用者: `demo-user`

GPU の情報とデバイス割当だけがダミーです。Docker、SSH、作業ファイル、Web 認証、中継、返却時の削除は実際に動きます。利用者の操作は後述の「GPU を借りて使う」と同じです。

新規デモでは RAM 合計 4 GiB・ディスク合計 12 GiB の貸出枠を設定し、貸出時の初期値は RAM 2 GiB・ディスク 4 GiB です。既存のデモで実行中の貸出がある場合は、その状態を維持します。

作業を終えるときは、まずパネルから貸出を返却し、返却完了を確認してからターミナルで Ctrl+C を押します。プロセスを終了しただけでは実行中のコンテナを削除しません。

## 2. 中央サーバーを用意する

中央サーバーにもリポジトリを配置し、`cargo build --release` でバイナリを用意します。提供者 PC の Docker や GPU は中央サーバーには不要です。

### ホスト名と公開ポート

この例では中央のドメインを `gpu.example.org` とします。実際のドメインに置き換えてください。

| ポート | 用途 |
| --- | --- |
| 443/TCP | WebUI と提供 PC の HTTPS API |
| 19000/TCP | 提供 PC が接続する中継制御用 SSH |
| 19001〜19999/TCP | 貸出ごとにランダムに割り当てる通信ポート |

1 回の貸出で中継に使うのは最大 4 ポートです。19000 は利用者の SSH 接続先ではありません。利用者はパネルに表示された割当済みポートへ接続します。

`examples/control.env` を `.control-state/control.env` にコピーし、ドメインを設定します。

```bash
mkdir -p .control-state
cp examples/control.env .control-state/control.env
```

```dotenv
GPUPOD_ALLOWED_HOSTS=gpu.example.org
GPUPOD_CSRF_ORIGINS=https://gpu.example.org
GPUPOD_RELAY_HOST=gpu.example.org
GPUPOD_TRUST_PROXY=1
```

`GPUPOD_TRUST_PROXY=1` は、Nginx 等が `X-Forwarded-Proto` を上書きする構成で使います。以下の準備・起動コマンドを実行するシェルにも環境変数を読み込みます。設定ファイルは管理者が用意したものを使用してください。

```bash
set -a
. .control-state/control.env
set +a
target/release/gpupod-control migrate
target/release/gpupod-control init-control
target/release/gpupod-control sync-templates
target/release/gpupod-control create-superuser
```

`init-control` は中央中継のホスト鍵を用意します。`.control-state/` の DB と鍵は中央の再起動後も保持します。

### Web と中継を起動する

環境変数を読み込んだターミナルで Web サーバーを起動します。

```bash
target/release/gpupod-control serve --bind 127.0.0.1:8000
```

別のターミナルでも同じ環境変数を読み込み、中継を起動します。

```bash
set -a
. .control-state/control.env
set +a
target/release/gpupod-control runrelay
```

HTTPS のリバースプロキシには `examples/nginx.conf` を使用できます。ドメイン、証明書、静的ファイルの配置先を実環境に合わせてから導入してください。回線測定用 API の location も含めます。

ブラウザーから `https://gpu.example.org/` を開き、作成した管理者でログインします。簡易管理画面の `/admin/` から利用者アカウントを作成し、通常の利用者にはスタッフ権限を付けません。

### 提供 PC を登録する

管理パネルの「提供 PC 管理」で名前を入力して登録します。ダウンロードした `node-….secret.json` を、その PC の提供者に渡します。以下では `node-provider.secret.json` と呼びます。

このファイルには PC 専用の認証情報が含まれます。利用者へ配ったり、公開リポジトリへ追加したりせず、その PC に配置します。

## 3. 提供 PC を用意する

提供 PC にもリポジトリを配置して `cargo build --release` を実行し、Docker Engine と対象 GPU のドライバー・コンテナ実行環境を準備します。NVIDIA、AMD、Intel で必要なホスト側の環境は異なります。

```bash
chmod 600 node-provider.secret.json
target/release/gpupod-agent devices
target/release/gpupod-tools doctor
```

`devices` で GPU が表示され、`doctor` で Docker に接続できることを確認します。必要なら `--devices` で提供者所有の GPU 定義を使用できます。

### 使用するテンプレートを準備する

Ubuntu 24.04 / 26.04 はどちらも次のコマンドでビルドします。

```bash
target/release/gpupod-tools build-images
```

PyTorch も使う場合は GPU 環境に応じて追加します。

| ビルド引数 | テンプレート ID | 用途 |
| --- | --- | --- |
| `--pytorch cpu` | `pytorch-cpu` | CPU 実行・GPU なしの検証 |
| `--pytorch cuda` | `pytorch-cuda` | NVIDIA |
| `--pytorch rocm` | `pytorch-rocm` | AMD |
| `--pytorch xpu` | `pytorch-xpu` | Intel |

```bash
target/release/gpupod-tools build-images --pytorch cuda
```

複数の引数を同時に指定することもできます。イメージをレジストリから配布する場合は、中央と提供 PC の両方のカタログで同じイメージ参照を使用します。

利用者は中央で有効になっている承認済みテンプレートを選択します。中央と提供 PC の定義が一致するものだけが対象です。提供 PC に必要なイメージがないと、起動に失敗します。

### ディスク・RAM の提供枠を設定する

受付を ON にする前に、提供 PC 全体で貸し出す容量と、貸出 1 件あたりの初期値を設定します。以下は RAM 合計 8 GiB・ディスク合計 100 GiB、初期値 RAM 2 GiB・ディスク 25 GiB の例です。提供者自身が使う分を除いた容量に置き換えてください。

```bash
target/release/gpupod-agent resources --memory-gib 8 --disk-gib 100 \
  --default-memory-gib 2 --default-disk-gib 25
```

利用者はこの枠内で予約量を選びます。起動待ちも予約に含め、すべての貸出の合計が提供枠を超える要求は拒否します。設定は状態 DB に保存するため、起動のたびに指定する必要はありません。変更するときは受付 OFF にして、すべての貸出が返却されるのを待ちます。

RAM の予約量をコンテナの使用量上限にします。ディスク予約は貸出間の容量配分の管理で、物理的な事前確保や使用量クォータは別設定です。提供 OFF でも実行中の貸出は維持します。利用終了でコンテナが停止すれば RAM は OS に戻り、専用の解放フラグや操作は必要ありません。予約台帳の割当は後片付け完了時に戻ります。

### 提供を開始する

```bash
target/release/gpupod-agent on
target/release/gpupod-agent daemon --config node-provider.secret.json
```

中央管理者もその PC の受付を ON にします。パネルでオンラインと表示され、GPU に対応するテンプレートが選択できることを確認します。

提供 PC は中央の HTTPS API と 19000/TCP に外向きに接続します。中央中継を使う場合、提供 PC 側の公開ポートは不要です。

状態の保存先を変えるときは、すべての提供側コマンドに同じ `--state-dir` を指定します。

```bash
target/release/gpupod-agent --state-dir /srv/gpupod/state status
target/release/gpupod-agent --state-dir /srv/gpupod/state on
target/release/gpupod-agent --state-dir /srv/gpupod/state daemon --config node-provider.secret.json
```

`--state-dir`、`--templates`、`--devices`、`--docker-socket`、`--mock-gpu` はサブコマンドより前に書きます。

## 4. GPU を借りて使う

### SSH 公開鍵を用意する

利用者の PC で SSH 鍵を作成します。すでに使用する鍵がある場合は、その公開鍵を使えます。

```bash
ssh-keygen -t ed25519 -f ~/.ssh/gpupod
cat ~/.ssh/gpupod.pub
```

画面に貼り付けるのは `.pub` の公開鍵です。秘密鍵は利用者の PC に置きます。

### パネルから借りる

1. 利用者アカウントでログインする。
2. 貸出可能な GPU を選ぶ。
3. Ubuntu または PyTorch のテンプレートを選ぶ。
4. 提供枠が設定されている PC では、RAM とディスクの予約量を GiB で指定する。
5. 中央中継、または提供 PC が対応している場合は直接接続を選ぶ。
6. SSH 公開鍵を貼り付けて開始する。
7. 状態が `running` になり、接続先が表示されるまで待つ。

### SSH とファイル転送

パネルに表示された接続先を使用します。次の `19427` は例なので、実際の割当ポートに置き換えます。

```bash
ssh -i ~/.ssh/gpupod -p 19427 pod@gpu.example.org
```

作業ファイルはコンテナ内の `/workspace` に置きます。利用者の PC からファイルを送る例です。scp のポート指定は大文字の `-P` です。

```bash
scp -i ~/.ssh/gpupod -P 19427 ./train.py pod@gpu.example.org:/workspace/
scp -i ~/.ssh/gpupod -P 19427 -r pod@gpu.example.org:/workspace/results ./results
```

PyTorch テンプレートには Python と PyTorch が入っています。SSH 接続後、次のコマンドで確認できます。

```bash
cd /workspace
/opt/venv/bin/python /usr/local/bin/torch-smoke.py --require-gpu
/opt/venv/bin/python train.py
```

GPU なしのデモや CPU テンプレートでは、確認コマンドを `--device cpu` にします。Ubuntu の最小テンプレートには Python や PyTorch を含めていません。

22、8888、6006、8080/TCP の接続先を用意していますが、初めから起動するのは SSH だけです。Web アプリは利用者が起動し、アプリ側の認証も設定します。

### 作業を終えて返却する

必要な成果物を手元へコピーしてから、パネルで「返却してデータを削除」を実行します。提供 PC から削除完了の通知が届くと返却が完了します。

返却後はコンテナと `/workspace` の内容を削除します。提供 PC がオフライン、または後片付けに失敗している場合は、予約を維持したまま完了を待ちます。

## 5. 提供中の操作

| 操作 | コマンド |
| --- | --- |
| 状態を見る | `target/release/gpupod-agent status` |
| 統計を見る | `target/release/gpupod-agent stats` |
| 新規受付を止め、実行中は継続 | `target/release/gpupod-agent off` |
| 受付も実行も停止し、作業データを削除 | `target/release/gpupod-agent off --mode stop` |
| 受付を再開 | `target/release/gpupod-agent on` |
| 後片付けを再確認 | `target/release/gpupod-agent reconcile` |

中央管理者も「提供 PC 管理」から ON、drain、stop を要求できます。提供 PC の手動 OFF は、自動検知が idle になっても解除されません。

`off` は `off --mode drain` と同じ動作です。OFF にしただけで利用中のコンテナやメモリを回収することはありません。貸出の終了・返却に従って解放します。

### 自動受付制御を使う

```bash
target/release/gpupod-tools auto-lend --mode drain --idle-seconds 60
```

提供者の GPU 使用と借用コンテナの使用を区別し、連続した idle 時間が経過してから受付を可能にします。検出できない場合は busy として扱います。必要なときだけ動かす別プロセスです。

### 実測性能と問い合わせ用ログ

```bash
target/release/gpupod-tools benchmark --disk-path /srv/gpupod-benchmark
target/release/gpupod-tools benchmark --network --config node-provider.secret.json
target/release/gpupod-tools support-bundle --config node-provider.secret.json
```

ディスク測定には fio 3.x が必要です。Docker の作業データと同じファイルシステム上の、書込可能な測定用ディレクトリを指定します。回線測定は提供 PC と中央サーバーの間の速度で、利用者パネルに測定日時とともに表示します。

問い合わせ用アーカイブは既定で `support-bundles/` に作成します。自動アップロードは行いません。ログを追加する方法や伏字処理の範囲は [README](../README.md#問い合わせ用の診断ログ) に記載しています。

## 6. 再起動後も常駐させる

systemd 用のファイルを生成できます。`/opt/gpupod` は配置したプロジェクトのパス、`gpupod` / `provider` は実際の Linux ユーザーに置き換えます。

```bash
target/release/gpupod-tools systemd-units control --user gpupod \
  --project /opt/gpupod --state-dir /opt/gpupod/.control-state \
  --output-dir .control-state/units
target/release/gpupod-tools systemd-units provider --user provider \
  --project /opt/gpupod --state-dir /srv/gpupod/state \
  --config /etc/gpupod/node-provider.secret.json --output-dir .provider-state/units
```

生成したファイルのユーザー、パス、権限を確認し、管理者が `/etc/systemd/system/` へ配置します。提供用ユーザーには Docker ソケットの操作権限と状態ディレクトリの書込権限が必要です。中央の環境変数は指定した状態ディレクトリ内の `control.env` に置きます。

中央サーバーで有効にするサービスは次の 2 つです。

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now gpupod-web.service gpupod-relay.service
```

提供 PC では次を有効にします。自動検知を使う場合だけ `gpupod-auto.service` も導入します。

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now gpupod-provider.service
```

提供サービスの再起動では実行中のコンテナを維持します。PC や Docker 自体の停止時には実行も止まります。停止後にコンテナが終了していた場合は、エージェントが後片付けを行います。

## 7. よくある確認箇所

| 症状 | 確認すること |
| --- | --- |
| 提供 PC が表示されない・オフライン | 提供エージェント、中央 URL、機材キー、HTTPS 接続 |
| GPU はあるが借りられない | 中央と提供側の受付 ON、対応テンプレート、GPU の予約・後片付け状態 |
| 予約可能な資源がない | 起動待ち・実行中・後片付け中の RAM / ディスク予約量、提供枠の設定 |
| テンプレートを選択できない | 中央と提供 PC のカタログが一致し、中央で有効になっているか |
| 起動に失敗する | イメージの有無、GPU ドライバー・デバイス権限、Docker、パネルのエラー |
| SSH に接続できない | 状態が running か、パネルのポート・接続先、対応する秘密鍵を使っているか |
| 中央中継だけつながらない | 中央の 19000〜19999/TCP、提供 PC の外向き接続、中継サービス |
| 直接接続だけつながらない | 提供 PC への到達性、DNS、NAT/VPN、`direct_host` と `direct_bind` |
| 返却が完了しない | 提供 PC がオンラインか、Docker の後片付けが成功しているか |

より詳しい設定は [README](../README.md) の gVisor、直接接続、ミラー、署名付き更新の説明を参照してください。