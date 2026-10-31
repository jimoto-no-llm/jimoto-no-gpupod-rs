// ビルド時に在庫を取得する。GPUPOD_INVENTORY_URL が無ければ空在庫でビルドする。
// 形: { providers, offered_gpus, available_gpus, gpus: [{ name, vendor, vram, provider, status, available }] }
import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = dirname(dirname(fileURLToPath(import.meta.url)));
const out = join(root, "src/data/inventory.json");
const empty = { providers: 0, offered_gpus: 0, available_gpus: 0, gpus: [] };

const url = process.env.GPUPOD_INVENTORY_URL;
if (!url) {
  writeFileSync(out, JSON.stringify(empty, null, 2) + "\n");
  console.log("inventory: GPUPOD_INVENTORY_URL 未設定のため空在庫でビルドします");
  process.exit(0);
}

try {
  const response = await fetch(url, { signal: AbortSignal.timeout(10_000) });
  if (!response.ok) throw new Error("HTTP " + response.status);
  const data = await response.json();
  const gpus = Array.isArray(data.gpus) ? data.gpus.slice(0, 60) : [];
  writeFileSync(
    out,
    JSON.stringify(
      {
        providers: Number(data.providers) || 0,
        offered_gpus: Number(data.offered_gpus) || 0,
        available_gpus: Number(data.available_gpus) || 0,
        gpus,
      },
      null,
      2,
    ) + "\n",
  );
  console.log("inventory: " + gpus.length + " 件を取得しました");
} catch (error) {
  writeFileSync(out, JSON.stringify(empty, null, 2) + "\n");
  console.warn("inventory: 取得に失敗したため空在庫でビルドします: " + error.message);
}
