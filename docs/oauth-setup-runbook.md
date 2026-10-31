# OAuth 実設定の運用手順（本日から動かすため）

## いま必要な作業（運営側・5分）

1. Discord Developer Portal でアプリを作る
   - https://discord.com/developers/applications
   - New Application → 名前: Jimoto GPU Pod
   - OAuth2 → Redirects: `https://<ドメイン>/oauth/discord/callback`
   - Scopes: identify email guilds
   - Client ID と Secret をメモる

2. Discord サーバーの Bot 設定 → OAuth2 → scopes 含め bot の invite link 取得
   （Bot をサーバー追加し、Bot Token も gpupod-control に渡す）

## 追加で必要な環境変数（既存のものから）

```dotenv
# 今あるものに追加
GPUPOD_DISCORD_BOT_TOKEN=xxxx    # ロール確認や member 引き出し用
```

## 実装が必要な変更

```rust
// oauth.rs の callback 時:
//   1. トークン交換の成功後、まず /users/@me の情報取得
//   2. guild member check は /users/@me/guilds/{guild}/member に Bot Token で呼ぶ
//   → ここでロール接合を確認し、fail すると 403（現在は Bot token なしの guilds API 方式）
```

この変更の後に Community プランの判定が Bot 側から行える。

## 作業日現実の求人

サービス化には Web UI も「生きている」形に:

- ドメイン: gpupod.example.org
- VPS: さくら VPS 1G / ConoHa 1G (月 5,000円 ~ 10,000円)
- オペレーター: Discord サーバーのロール「運営」
- 提供 PC: Discord 上で募集し、OK した人に systemd ユニットを渡す

## 質問（回答お願いします）

1. ドメインは確定しましたか？（何もなければ jp ドメインで gpupod.jp があるか）
2. VPS プロバイダは ConoHa でよいですか？
3. Discord 招待リンクは発行可能？
