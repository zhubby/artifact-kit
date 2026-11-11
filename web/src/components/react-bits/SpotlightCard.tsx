"use client";

// Adapted from React Bits SpotlightCard, pinned in provenance.json.
import {
  useEffect,
  useRef,
  type HTMLAttributes,
  type PointerEvent,
} from "react";
import { usePreferences } from "@/components/providers";
import { cn } from "@/lib/utils";

export default function SpotlightCard({
  children,
  className,
  as: Component = "div",
  ...props
}: HTMLAttributes<HTMLElement> & { as?: "div" | "article" }) {
  const root = useRef<HTMLElement | null>(null);
  const frame = useRef(0);
  const { motion } = usePreferences();
  useEffect(() => {
    if (!motion) root.current?.style.removeProperty("--spotlight-presence");
    return () => cancelAnimationFrame(frame.current);
  }, [motion]);
  function move(event: PointerEvent<HTMLElement>) {
    if (!motion || event.pointerType !== "mouse") return;
    const { clientX, clientY } = event;
    cancelAnimationFrame(frame.current);
    frame.current = requestAnimationFrame(() => {
      const node = root.current;
      if (!node || document.hidden) return;
      const rect = node.getBoundingClientRect();
      node.style.setProperty("--spotlight-x", `${clientX - rect.left}px`);
      node.style.setProperty("--spotlight-y", `${clientY - rect.top}px`);
      node.style.setProperty("--spotlight-presence", "1");
    });
  }
  return (
    <Component
      ref={(node) => {
        root.current = node;
      }}
      className={cn("spotlight-card", className)}
      onPointerMove={move}
      onPointerLeave={() =>
        root.current?.style.removeProperty("--spotlight-presence")
      }
      {...props}
    >
      <span className="spotlight-fill" aria-hidden="true" />
      {children}
      <span className="spotlight-edge" aria-hidden="true" />
    </Component>
  );
}
