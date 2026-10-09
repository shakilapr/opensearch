/**
 * @file searchErrors.ts
 * @description Typed search-layer failures. These replace the removed
 * fabricated-result fallback: when the local search API cannot answer, the
 * service surfaces this error so the UI can render an honest, actionable state
 * instead of inventing results. Zero use of `any`.
 */

export type SearchFailureReason = 'unreachable' | 'http_error' | 'malformed_response';

/**
 * Raised by the client/service whenever a search cannot be answered by the real
 * API (transport failure, non-2xx status, or an unusable response body).
 */
export class SearchUnavailableError extends Error {
  public readonly baseUrl: string;
  public readonly reason: SearchFailureReason;
  public readonly httpStatusCode: number | null;

  public constructor(
    baseUrl: string,
    reason: SearchFailureReason,
    httpStatusCode: number | null = null
  ) {
    super('The local search server is not reachable at the configured base URL.');
    this.name = 'SearchUnavailableError';
    this.baseUrl = baseUrl;
    this.reason = reason;
    this.httpStatusCode = httpStatusCode;
  }
}

/** A short, human-readable explanation of why a search failed. */
export function describeSearchFailure(error: SearchUnavailableError): string {
  switch (error.reason) {
    case 'http_error':
      return error.httpStatusCode !== null
        ? `The local search server returned HTTP ${error.httpStatusCode}.`
        : 'The local search server returned an error response.';
    case 'malformed_response':
      return 'The local search server returned a response the UI could not read.';
    default:
      return 'No response was received from the local search server.';
  }
}
