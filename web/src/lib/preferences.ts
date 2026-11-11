export type Locale = "zh-CN" | "en";
export type ThemeMode = "system" | "dark" | "light";
export const localeCookie = "artifactory-locale";
export const effectsStorage = "artifactory-effects";
export const themeStorage = "artifactory-theme";
export function isLocale(value: unknown): value is Locale {
  return value === "zh-CN" || value === "en";
}
