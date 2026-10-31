import type { AnchorHTMLAttributes, ButtonHTMLAttributes } from "react";
import { cn } from "@/lib/cn";

type Variant = "primary" | "outline" | "ghost";
type Size = "sm" | "md" | "lg";

const variants: Record<Variant, string> = {
  primary:
    "bg-ink text-white border-ink hover:bg-[#2b2b2e] shadow-[inset_0_1px_0_rgba(255,255,255,0.1)]",
  outline: "bg-surface text-ink border-line-2 hover:bg-raised",
  ghost: "bg-transparent border-transparent text-ink-2 hover:text-ink hover:bg-raised",
};

const sizes: Record<Size, string> = {
  sm: "h-8 px-3 text-[13px]",
  md: "h-9 px-4 text-sm",
  lg: "h-10 px-5 text-sm",
};

const base =
  "inline-flex items-center justify-center gap-2 whitespace-nowrap rounded-md border font-medium leading-none transition-colors focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-2 disabled:opacity-50";

export function Button(props: ButtonHTMLAttributes<HTMLButtonElement> & { variant?: Variant; size?: Size }) {
  const { variant = "primary", size = "md", className, ...rest } = props;
  return <button className={cn(base, variants[variant], sizes[size], className)} {...rest} />;
}

export function ButtonLink(props: AnchorHTMLAttributes<HTMLAnchorElement> & { variant?: Variant; size?: Size }) {
  const { variant = "primary", size = "md", className, ...rest } = props;
  return <a className={cn(base, variants[variant], sizes[size], className)} {...rest} />;
}
