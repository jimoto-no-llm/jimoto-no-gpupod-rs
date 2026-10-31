# ADR 0001: Web 構成（Astro + TanStack）と言語分担

- 状態: 提案（2026-10-31）
- 決定者: 運営
- 関連: docs/roadmap.md

## 背景

有志が GPU を貸し出すサービスとして、公開サイト（LP・料金・提供者募集・規約・使い方）と、
ログイン後のダッシュボード（GPU の検索・絞り込み、貸出の作成/停止、状態表示）が必要になった。
現状は Rust の axum + askama で両方を配信している。公開サイトは静的でよい一方、
ダッシュボードは検索・絞り込み・状態更新が多く、フロントエンドの表現力と型安全が要る。

## 調査（imozuru）

原文は `~/research/gpu-marketplace-web-stack/`（report.html に抜き書き、map.md / log.md に経緯）。
公式ドキュメントで確認した事実:

- Astro の islands は `client:*` を付けたコンポーネントだけ水和し、残りは静的配信
  （astro-islands.md:69,77）
- Astro の SSR はアダプタ必須。Node / Netlify / Vercel / Cloudflare が公式（astro-ssr.md:9,29）
- TanStack Start は Router を土台にしたフルスタックで full-document SSR と server functions
  （tanstack-start.md:35,49）
- TanStack Router は search params を型安全な第一級の状態として扱う（tanstack-router.md:147,151）
- TanStack Query は server state の取得・キャッシュ・同期（tanstack-query.md:33,69）
- TanStack Table / Form は headless（tanstack-llms.md:22,24）
- **Astro 公式に TanStack の統合は無い**。TanStack Query の公式サンプルは Solid 版のみで、
  React 版の Astro サンプルは存在しない（TanStack 公式の Astro 統合ページは 404）
- Astro の islands 間では **React Context を共有できない**。共有が要るなら Nano Stores
  （Astro 公式レシピ）
- TanStack Start の Query ガイドは「QueryClient をモジュールスコープに置くな」と明記
  （リクエスト間でキャッシュが混ざる）
- Astro の server islands（server:defer）は props がシリアライズ可能である必要がある
- Astro のビルド後に TanStack Router のルートが動かないという報告がある（TanStack discussion #422）
- TanStack Router の公式インストール手順は Vite / Rspack / Webpack / Esbuild / Router CLI のみで、
  Astro の手順は無い。コミュニティ実装は catch-all + client:only + basepath で回避している
- Astro の islands は独立してハイドレートするため、ルートの QueryClientProvider は期待通り動かない
  （island ごとに QueryClient を持つ）
- `output: 'hybrid'` は Astro v5 で廃止されている

## 決定

### 1. 公開サイトは Astro（静的）＋ 最小の islands

- 対象: `/` `/pricing/` `/providers/` `/how/` `/terms/` `/privacy/`
- 静的生成。インタラクティブなのは在庫ボードとログインボタンだけを islands にする
- コンテンツは Astro の content collections で管理し、ビルド時に検証する

### 2. ダッシュボードは Astro SSR ＋ React islands

- Astro はアダプタ（Node）で SSR。Rust の API をサーバー側で呼ぶ
- 一覧は **TanStack Table**（headless）で列・ソート・絞り込みを実装
- サーバー状態は **TanStack Query**（ポーリング・楽観更新・再取得）
- 貸出作成フォームは **TanStack Form**
- **TanStack Router は使わない**。Astro がファイルベースのルーティングを持つため、
  二重ルーターは遷移の所有権が曖昧になり、失败の温床になる
- **TanStack Start も使わない**（Astro と役割が重複する）。将来、操作の多い SPA に
  する必要が出たときだけ再検討する

### 3. Rust は「真実の源」の API とシステム領域に残す

| 領域 | 言語 | 理由 |
| --- | --- | --- |
| 提供エージェント（Docker/SSH/監視/更新） | Rust | 常駐・低メモリ・システム境界 |
| SSH 中継（russh） | Rust | 非同期 I/O と接続管理 |
| 中央 API（貸出・ノード・認証・状態） | Rust | 既存資産。JSON API に徹する |
| CLI 補助ツール | Rust | 単一バイナリ配布 |
| 公開サイト・ダッシュボード | TypeScript（Astro/React/TanStack） | UI の型安全と生産性 |
| BFF（Cookie セッション、OAuth callback） | TypeScript（Astro のサーバー側） | Web と同一プロセスで扱う |
| ベンチ集計・分析・可視化 | Python | データ処理の生態系 |
| スキーマ・運用手順 | SQL / Bash | 既存の運用に合わせる |

Rust の API は `/api/v1/` に JSON で公開し、Web 側は OpenAPI から型を生成して食い違わないようにする。

### 4. 認証の置き場所

- Discord OAuth の callback は Astro のサーバー側で受ける（Cookie を Web のドメインに閉じる）
- Astro は内部トークンで Rust API を呼ぶ
- 移行期間は Rust 側の既存 OAuth を残し、Astro のログインと併存させる

## 構成（リポジトリ）

```
apps/site/      Astro（公開サイト。静的）
apps/console/   Astro SSR（ダッシュボード。React islands + TanStack）
crates/         Rust（agent / control / tools / common）
scripts/        運用（Bash）
```

## 失敗しやすい点（先に手当てする）

1. **二重ルーター**: Astro と TanStack Router を併用しない（本条で禁止。ビルド後にルートが
   動かない報告もある）
2. **islands 間の Context**: React Context は islands をまたげない。共有状態は Nano Stores
   か、URL（search params）に置く
3. **QueryClient のスコープ**: SSR ではリクエストごとに生成する。モジュールスコープに置くと
   別リクエストのキャッシュが混ざる
4. **過剰な水和**: `client:load` を乱用しない。`client:visible` / `client:idle` を既定にする
5. **server:defer の props**: シリアライズ可能な値だけを渡す（関数は不可）
6. **秘密の混入**: SSR のサーバーコードにのみ秘密を置く。クライアント island へ渡さない
7. **Cookie のドメイン分離**: サイトと API を別ホストにすると SameSite で切れる。同一オリジン配下に置く
8. **型のずれ**: OpenAPI から生成し、CI で差分を検出する

## 判断の分岐点（Astro をやめる条件）

次のどれかに当てはまったら、Astro をやめて **TanStack Start 一本**に切り替える。

- ダッシュボードで画面遷移をまたぐクライアント状態が増え、URL と無関係な状態管理が必要になった
- islands をまたぐ Context 共有が常態化した
- 認証後の画面が 20 画面を超え、ファイルベースのルーティングでは粒度が合わなくなった

TanStack Start は 2026-10-13 に 1.0 stable。選ぶ場合は Astro を併用しない（相互の公式言及が無い）。

## 段階的な移行

1. 公開サイトを Astro へ（Rust の LP は残す。リバースプロキシで振り分け）
2. ダッシュボードを Astro SSR + islands へ。Rust は API に徹する
3. 必要なら TanStack Start を検討（現時点では不要）

## 参考（マーケットプレイスの画面）

- Vast.ai の検索画面は「上部と左のフィルタ」「オファーカード」「マシン階層（Unverified / Verified /
  Secure Cloud）」で構成されている（vast-find-rent.md:19,23,29,31-33）
- JANCTION は需要者と提供者をつなぐ GPU マーケットプレイスとして、スペックと価格の比較を掲げている
- 画面はこの構成を踏襲し、階層（検証済み・未検証）は「提供者の信頼度」として表示する

## 未解決

- 第三者の実装事例と失敗事例は未取得（imozuru の次周で当たる）
- ホスティング（Cloudflare / Vercel / VPS 上の Node）は運用要件で決める