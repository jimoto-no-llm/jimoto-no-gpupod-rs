import { useMemo, useState } from "react";
import { Card, CardHeader, CardTitle, CardMeta, CardFoot } from "@/components/ui/card";
import { StatusBadge } from "@/components/ui/status-badge";

export type Gpu = {
  name: string;
  vendor: string;
  vram: string;
  provider: string;
  status: string;
  available: boolean;
};

export type Inventory = {
  providers: number;
  offered_gpus: number;
  available_gpus: number;
  gpus: Gpu[];
};

type Status = "free" | "busy" | "off";
const order: Record<Status, number> = { free: 0, busy: 1, off: 2 };

const statusOf = (g: Gpu): Status =>
  g.available ? "free" : g.status === "受付停止" ? "off" : "busy";

type SortKey = "name" | "vram" | "provider" | "status";

const HEADERS: { key: SortKey; label: string; num?: boolean }[] = [
  { key: "name", label: "GPU" },
  { key: "vram", label: "VRAM", num: true },
  { key: "provider", label: "貸し手" },
  { key: "status", label: "状態" },
];

export default function GPUBoard({ initial }: { initial: Inventory }) {
  const [sort, setSort] = useState<{ key: SortKey; desc: boolean } | null>(null);
  const [onlyFree, setOnlyFree] = useState(false);

  const gpus: Gpu[] = initial?.gpus ?? [];
  const rows = useMemo(() => {
    const base = onlyFree ? gpus.filter((g) => g.available) : gpus;
    if (!sort) return base;
    const s = [...base].sort((a, b) => {
      if (sort.key === "status") return order[statusOf(a)] - order[statusOf(b)];
      if (sort.key === "vram") {
        const n = (v: string) => parseFloat(v) || 0;
        return n(a.vram) - n(b.vram);
      }
      const k = sort.key as "name" | "provider";
      return a[k].localeCompare(b[k], "ja");
    });
    return sort.desc ? s.reverse() : s;
  }, [gpus, sort, onlyFree]);

  const toggle = (key: SortKey) =>
    setSort((prev) =>
      prev?.key === key ? (prev.desc ? null : { key, desc: true }) : { key, desc: false },
    );

  return (
    <Card>
      <CardHeader>
        <CardTitle>いまの在庫</CardTitle>
        <div className="flex items-center gap-3">
          <label className="flex cursor-pointer items-center gap-1.5 text-xs text-ink-2">
            <input
              type="checkbox"
              checked={onlyFree}
              onChange={(e) => setOnlyFree(e.target.checked)}
              className="size-3.5 accent-ok"
            />
            空きだけ表示
          </label>
          <CardMeta>{initial?.providers ?? 0}台の提供PC</CardMeta>
        </div>
      </CardHeader>

      <div className="overflow-x-auto">
        <table className="w-full text-sm">
          <thead>
            <tr className="border-b border-line bg-bg-sub">
              {HEADERS.map((h) => (
                <th
                  key={h.key}
                  scope="col"
                  onClick={() => toggle(h.key)}
                  className={
                    "h-9 cursor-pointer select-none px-4 text-left text-xs font-medium text-ink-3 transition-colors hover:text-ink " +
                    (h.num ? "text-right" : "")
                  }
                >
                  {h.label}
                  <span className="ml-1 text-[10px]" aria-hidden="true">
                    {sort?.key === h.key ? (sort.desc ? "▼" : "▲") : ""}
                  </span>
                </th>
              ))}
            </tr>
          </thead>
          <tbody>
            {rows.length === 0 ? (
              <tr>
                <td colSpan={4} className="border-b-0 px-4 py-10 text-center">
                  <p className="text-sm text-ink-2">
                    {onlyFree ? "空きGPUはありません「空きだけ表示」を解除してください。" : "いま貸し出せるGPUはありません"}
                  </p>
                  {!onlyFree && (
                    <p className="mt-1 text-xs text-ink-3">提供PCが登録されると、この表に並びます。</p>
                  )}
                </td>
              </tr>
            ) : (
              rows.map((g, i) => (
                <tr key={g.name + "-" + i} className="border-b border-line last:border-b-0 transition-colors hover:bg-raised/70">
                  <td className="h-11 px-4 align-middle">
                    <div className="flex flex-col items-start gap-0.5">
                      <span className="font-mono text-[13px] text-ink">{g.name}</span>
                      <span className="text-[11.5px] text-ink-3">{g.vendor}</span>
                    </div>
                  </td>
                  <td className="h-11 px-4 text-right align-middle font-mono text-xs tabular-nums text-ink-2">
                    {g.vram}
                  </td>
                  <td className="h-11 max-w-[160px] truncate px-4 align-middle text-[12.5px] text-ink-2">
                    {g.provider}
                  </td>
                  <td className="h-11 px-4 align-middle">
                    <StatusBadge status={statusOf(g)}>
                      {g.available ? "貸出可能" : g.status === "受付停止" ? "受付停止" : "利用中"}
                    </StatusBadge>
                  </td>
                </tr>
              ))
            )}
          </tbody>
        </table>
      </div>

      <CardFoot>表示はビルド時点のスナップショットです。IPアドレスと内部IDは公開しません。</CardFoot>
    </Card>
  );
}
