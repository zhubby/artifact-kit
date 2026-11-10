import { clsx, type ClassValue } from "clsx";
import { twMerge } from "tailwind-merge";

export function cn(...inputs: ClassValue[]) { return twMerge(clsx(inputs)); }
export function short(value?: string | null) { return value ? value.slice(0, 8) : "—"; }
export function date(value?: string | null) { return value ? new Intl.DateTimeFormat("zh-CN", { month: "2-digit", day: "2-digit", hour: "2-digit", minute: "2-digit", hour12: false }).format(new Date(value)) : "—"; }
export function bytes(value?: number) { if (value === undefined) return "—"; if (value < 1024) return `${value} B`; return `${(value / 1024 ** Math.floor(Math.log(value) / Math.log(1024))).toFixed(1)} ${["B", "KiB", "MiB", "GiB", "TiB"][Math.floor(Math.log(value) / Math.log(1024))]}`; }
export function elapsed(start?: string | null, end?: string | null) { if (!start) return "—"; const seconds = Math.max(0, Math.floor(((end ? new Date(end).getTime() : Date.now()) - new Date(start).getTime()) / 1000)); return seconds < 60 ? `${seconds}s` : `${Math.floor(seconds / 60)}m ${seconds % 60}s`; }
