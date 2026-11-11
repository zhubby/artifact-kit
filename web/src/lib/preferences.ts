export type Locale = "zh-CN" | "en";
export type ThemeMode = "system" | "dark" | "light";
export const localeCookie = "ak-locale";
export const effectsStorage = "ak-effects";
export const themeStorage = "ak-theme";
export function isLocale(value: unknown): value is Locale {
  return value === "zh-CN" || value === "en";
}
