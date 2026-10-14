import type { DbError } from '@/bindings/DbError';

/** An error raised by a backend command, carrying the structured `DbError`. */
export class AppError extends Error {
  readonly info: DbError;

  constructor(info: DbError) {
    super(info.message);
    this.name = 'AppError';
    this.info = info;
  }
}

function isDbError(value: unknown): value is DbError {
  return typeof value === 'object' && value !== null && 'kind' in value && 'message' in value;
}

/** Normalises anything thrown into an `AppError`. */
export function toAppError(error: unknown): AppError {
  if (error instanceof AppError) return error;
  if (isDbError(error)) return new AppError(error);
  const message = error instanceof Error ? error.message : String(error);
  return new AppError({ kind: 'internal', code: null, message, detail: null, hint: null, action: null, position: null });
}

export function errorMessage(error: unknown): string {
  return toAppError(error).message;
}
