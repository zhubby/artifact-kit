type LocalErrorKey =
  | "common.networkFailed"
  | "common.httpFailed"
  | "common.invalidJson"
  | "common.operationFailed"
  | "common.requestFailed"
  | "login.failed"
  | "register.failed"
  | "register.invalidUsername"
  | "register.invalidPassword"
  | "register.invalidFields"
  | "register.passwordMismatch"
  | "register.usernameTaken"
  | "register.forbidden"
  | "register.rateLimited"
  | "register.configFailed";

// Retain translation keys until rendering so existing errors follow the locale.
export class LocalizedError extends Error {
  constructor(
    readonly key: LocalErrorKey,
    readonly values: Record<string, string | number> = {},
  ) {
    super(key);
  }
}

export function asError(
  error: unknown,
  fallback: LocalErrorKey = "common.operationFailed",
) {
  return error instanceof Error ? error : new LocalizedError(fallback);
}
