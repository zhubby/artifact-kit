"use client";

import { useCallback, useEffect, useState } from "react";

export async function api<Type>(path: string, init: RequestInit = {}): Promise<Type> {
  const response = await fetch(`/api/v1${path}`, { ...init, credentials: "same-origin", headers: { "Content-Type": "application/json", "x-artifact-csrf": "1", ...init.headers }, cache: "no-store" });
  const body = await response.json().catch(() => ({ error: `服务暂不可用 (${response.status})` }));
  if (!response.ok) {
    if (response.status === 401 && path !== "/auth/login" && typeof window !== "undefined") window.location.replace("/login");
    throw new Error(body.error || `请求失败 (${response.status})`);
  }
  return body as Type;
}

export function mutation<Type>(path: string, body: unknown, method = "POST") { return api<Type>(path, { method, body: JSON.stringify(body) }); }

export function useRemote<Type>(path: string | null, interval = 10000) {
  const [data, setData] = useState<Type>();
  const [error, setError] = useState<string>();
  const reload = useCallback(async () => {
    if (!path) return;
    try { setData(await api<Type>(path)); setError(undefined); } catch (error) { setError(error instanceof Error ? error.message : "读取失败"); }
  }, [path]);
  useEffect(() => { setData(undefined); void reload(); if (!path) return; const timer = setInterval(() => void reload(), interval); return () => clearInterval(timer); }, [reload, path, interval]);
  return { data, error, reload, setData };
}
