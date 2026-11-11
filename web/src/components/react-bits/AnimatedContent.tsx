"use client";

// Adapted from React Bits AnimatedContent, pinned in provenance.json.
import { useEffect, useRef, type HTMLAttributes } from "react";
import { gsap } from "gsap";
import { usePreferences } from "@/components/providers";

export default function AnimatedContent({
  children,
  replayKey = "mount",
  stagger = false,
  ...props
}: HTMLAttributes<HTMLDivElement> & { replayKey?: string; stagger?: boolean }) {
  const element = useRef<HTMLDivElement>(null);
  const played = useRef<string | null>(null);
  const { motion } = usePreferences();
  useEffect(() => {
    const root = element.current;
    if (!root || !motion || played.current === replayKey) return;
    played.current = replayKey;
    const context = gsap.context(() => {
      const targets =
        stagger && root.children.length ? Array.from(root.children) : [root];
      gsap.fromTo(
        targets,
        { y: 16, opacity: 0, filter: "blur(3px)" },
        {
          y: 0,
          opacity: 1,
          filter: "blur(0px)",
          duration: 0.45,
          stagger: stagger ? 0.06 : 0,
          ease: "power3.out",
          clearProps: "transform,opacity,filter",
        },
      );
    }, root);
    return () => context.revert();
  }, [motion, replayKey, stagger]);
  return (
    <div ref={element} {...props}>
      {children}
    </div>
  );
}
