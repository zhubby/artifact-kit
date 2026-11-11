"use client";

import dynamic from "next/dynamic";
import { useTheme } from "next-themes";
import { useEffect, useState } from "react";
import { usePreferences } from "@/components/providers";
import { cn } from "@/lib/utils";

const Aurora = dynamic(() => import("@/components/react-bits/Aurora"), {
  ssr: false,
});

export function Atmosphere({ className }: { className?: string }) {
  const { motion } = usePreferences();
  const { resolvedTheme } = useTheme();
  const [compact, setCompact] = useState(false);
  useEffect(() => {
    const query = matchMedia("(max-width: 760px)");
    const update = () => setCompact(query.matches);
    update();
    query.addEventListener("change", update);
    return () => query.removeEventListener("change", update);
  }, []);
  return (
    <div className={cn("atmosphere", className)} aria-hidden="true">
      <div className="atmosphere-static" />
      {motion && (
        <Aurora
          colorStops={["#3b9c77", "#d3bc73", "#407f71"]}
          amplitude={0.85}
          speed={0.5}
          layers={2}
          height={0.5}
          blend={0.65}
          glow={0.45}
          opacity={resolvedTheme === "light" ? 0.58 : 0.78}
          lightMode={resolvedTheme === "light"}
          mouseInteraction={false}
          dpr={compact ? 1 : 1.5}
          fps={compact ? 24 : 40}
        />
      )}
      <div className="atmosphere-grid" />
    </div>
  );
}
