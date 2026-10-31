import type { HTMLAttributes } from "react";
import { cn } from "@/lib/cn";

export function Card(props: HTMLAttributes<HTMLDivElement>) {
  const { className, ...rest } = props;
  return (
    <div
      className={cn(
        "rounded-xl border border-line-2 bg-surface shadow-[inset_0_1px_0_rgba(255,255,255,0.6)]",
        className,
      )}
      {...rest}
    />
  );
}

export function CardHeader(props: HTMLAttributes<HTMLDivElement>) {
  const { className, ...rest } = props;
  return (
    <div
      className={cn(
        "flex items-center justify-between gap-3 border-b border-line bg-raised px-4 py-2.5 rounded-t-xl",
        className,
      )}
      {...rest}
    />
  );
}

export function CardTitle(props: HTMLAttributes<HTMLHeadingElement>) {
  return <h3 className="text-[13px] font-medium text-ink" {...props} />;
}

export function CardMeta(props: HTMLAttributes<HTMLSpanElement>) {
  return <span className="font-mono text-xs text-ink-3" {...props} />;
}

export function CardBody(props: HTMLAttributes<HTMLDivElement>) {
  return <div className="px-4 py-3" {...props} />;
}

export function CardFoot(props: HTMLAttributes<HTMLParagraphElement>) {
  return (
    <p
      className="border-t border-line bg-bg-sub px-4 py-2 text-[11.5px] text-ink-3 rounded-b-xl"
      {...props}
    />
  );
}
