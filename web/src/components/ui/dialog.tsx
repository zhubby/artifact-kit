"use client";

import * as Dialog from "@radix-ui/react-dialog";
import { X } from "lucide-react";
import { useRef, type ReactNode } from "react";

export function Modal({ open, onOpenChange, title, description, children }: { open: boolean; onOpenChange: (open: boolean) => void; title: string; description: string; children: ReactNode }) {
  const previousFocus = useRef<HTMLElement | null>(null);
  return <Dialog.Root open={open} onOpenChange={onOpenChange}>
    <Dialog.Portal>
      <Dialog.Overlay className="fixed inset-0 z-40 bg-black/65 backdrop-blur-xs" />
      <Dialog.Content
        className="modal-panel fixed top-1/2 left-1/2 z-50 max-h-[90dvh] w-[calc(100%-2rem)] max-w-xl -translate-x-1/2 -translate-y-1/2 overflow-auto rounded-xl border border-border bg-card p-6 shadow-2xl"
        onOpenAutoFocus={() => { previousFocus.current = document.activeElement instanceof HTMLElement ? document.activeElement : null; }}
        onCloseAutoFocus={event => { event.preventDefault(); if (previousFocus.current?.isConnected) previousFocus.current.focus(); }}
      >
        <Dialog.Title className="text-lg font-medium">{title}</Dialog.Title>
        <Dialog.Description className="mt-2 pr-5 text-sm leading-6 text-muted-foreground">{description}</Dialog.Description>
        <Dialog.Close className="absolute top-4 right-4 rounded p-1 text-muted-foreground hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring" aria-label="关闭对话框"><X size={18}/></Dialog.Close>
        <div className="mt-6">{children}</div>
      </Dialog.Content>
    </Dialog.Portal>
  </Dialog.Root>;
}
