"use client";

import {
  useCallback,
  useEffect,
  useRef,
  useState,
  type SetStateAction,
} from "react";
import { asError, LocalizedError } from "@/lib/errors";

export async function api<Type>(
  path: string,
  init: RequestInit = {},
): Promise<Type> {
  let response: Response;
  try {
    response = await fetch(`/api/v1${path}`, {
      ...init,
      credentials: "same-origin",
      headers: {
        "Content-Type": "application/json",
        "x-artifactory-csrf": "1",
        ...init.headers,
      },
      cache: "no-store",
    });
  } catch {
    throw new LocalizedError("common.networkFailed");
  }
  const body = await response.json().catch(() => undefined);
  if (!response.ok) {
    if (
      response.status === 401 &&
      path !== "/auth/login" &&
      typeof window !== "undefined"
    )
      window.location.replace("/login");
    if (typeof body?.error === "string" && body.error)
      throw new Error(body.error);
    throw new LocalizedError("common.httpFailed", { status: response.status });
  }
  return body as Type;
}

export function mutation<Type>(path: string, body: unknown, method = "POST") {
  return api<Type>(path, { method, body: JSON.stringify(body) });
}

export function useRemote<Type>(path: string | null, interval = 10000) {
  const [snapshot, setSnapshot] = useState<{
    path: string | null;
    data?: Type;
    error?: Error;
  }>({ path });
  const currentPath = useRef(path);
  currentPath.current = path;
  const setData = useCallback(
    (value: SetStateAction<Type | undefined>) => {
      setSnapshot((previous) => ({
        path,
        data:
          typeof value === "function"
            ? (value as (previous: Type | undefined) => Type | undefined)(
                previous.path === path ? previous.data : undefined,
              )
            : value,
      }));
    },
    [path],
  );
  const reload = useCallback(async () => {
    if (!path) return;
    try {
      const data = await api<Type>(path);
      if (currentPath.current === path) setSnapshot({ path, data });
    } catch (error) {
      if (currentPath.current === path)
        setSnapshot((previous) => ({
          path,
          data: previous.path === path ? previous.data : undefined,
          error: asError(error, "common.requestFailed"),
        }));
    }
  }, [path]);
  useEffect(() => {
    void reload();
    if (!path) return;
    const timer = setInterval(() => void reload(), interval);
    return () => clearInterval(timer);
  }, [reload, path, interval]);
  return {
    data: snapshot.path === path ? snapshot.data : undefined,
    error: snapshot.path === path ? snapshot.error : undefined,
    reload,
    setData,
  };
}
