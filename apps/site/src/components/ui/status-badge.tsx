import type { HTMLAttributes } from "react";
import { cn } from "@/lib/cn";

type Status = "free" | "busy" | "off";

const statusCls: Record<Status, string> = {
  free: "border-ok/40 bg-ok-soft text-ok",
  busy: "border-line-2 bg-raised text-busy",
  off: "border-line bg-raised text-off",
};

export function StatusBadge(props: HTMLAttributes<HTMLSpanElement> & { status: Status }) {
  const { status, className, ...rest } = props;
  return (
    <span
      className={cn(
        "inline-flex items-center gap-1.5 rounded-full border px-2 py-0.5 text-xs leading-5 whitespace-nowrap",
        statusCls[status],
        className,
      )}
      {...rest}
    >
      <span
        aria-hidden="true"
        className={
          status === "free" ? "size-1.5 rounded-full bg-ok" : status === "busy" ? "size-1.5 rounded-full bg-busy" : "size-1.5 rounded-full bg-off"
        }
      />
      {rest.children}
    </span>
  );
}
