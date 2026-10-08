/**
 * @file hexxicaClient.ts
 * @description Real HTTP client for the local search-api (`POST /search`, `GET /health`).
 * ISO/IEC 25010 Fault-Tolerance and NASA Rule 7 (Input & Return Validation).
 * Zero use of `any`.
 */

import { assertBoundedString, assertNonNull } from '../core/assertions';
import { NodeHealthTelemetry, NodeStatusType, SearchResult, SearchSignal } from '../domain/models';

/** Default API base URL; override with the `VITE_API_BASE` env var. */
export const HEXXICA_DEFAULT_ENDPOINT = 'http://127.0.0.1:8080';
const NETWORK_TIMEOUT_MS = 6000;
const MAX_RESULTS = 100;

/** Resolve the configured API base URL from the Vite env, else the default. */
function resolveBaseUrl(): string {
  const envBase: string | undefined = import.meta.env?.VITE_API_BASE;
  if (typeof envBase === 'string' && envBase.trim().length > 0) {
    return envBase.trim();
  }
  return HEXXICA_DEFAULT_ENDPOINT;
}

interface RawSignal {
  readonly name: string;
  readonly value: number;
  readonly note: string;
}

/** A single search result as serialized by the Rust `SearchResult` contract. */
interface RawSearchResult {
  readonly doc_id: string;
  readonly title: string;
  readonly canonical_url: string;
  readonly snippet: string;
  readonly score: number;
  readonly source_id?: string | null;
  readonly published_at?: number | null;
  readonly fetched_at?: number | null;
  readonly lane?: string;
  readonly provenance?: string;
  readonly signals?: readonly RawSignal[];
}

interface RawSearchResponse {
  readonly results: readonly RawSearchResult[];
}

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null;
}

/** Validate an unknown payload as a `SearchResponse` without using `any`. */
function isSearchResponse(data: unknown): data is RawSearchResponse {
  if (!isObject(data)) {
    return false;
  }
  return Array.isArray(data.results);
}

function asString(value: unknown, fallback: string): string {
  return typeof value === 'string' ? value : fallback;
}

function asFiniteNumber(value: unknown, fallback: number): number {
  return typeof value === 'number' && Number.isFinite(value) ? value : fallback;
}

/** Coerce one raw signal entry into the domain `SearchSignal` shape. */
function mapSignal(raw: unknown): SearchSignal | null {
  if (!isObject(raw)) {
    return null;
  }
  const name = asString(raw.name, '');
  if (name.length === 0) {
    return null;
  }
  return {
    name,
    value: asFiniteNumber(raw.value, 0),
    note: asString(raw.note, ''),
  };
}

/** A unix-second timestamp rendered as ISO-8601, tolerating null/absent values. */
function toIsoOrNow(seconds: unknown): string {
  const s = asFiniteNumber(seconds, 0);
  if (s <= 0) {
    return new Date().toISOString();
  }
  return new Date(s * 1000).toISOString();
}

/** Hostname + path displayed under a result title, e.g. `doc.rust-lang.org › guide`. */
function buildDisplayUrl(url: string, hostname: string): string {
  try {
    const parsed = new URL(url);
    const path = parsed.pathname.replace(/\/$/, '');
    return path.length > 0 ? `${parsed.hostname}${path}` : parsed.hostname;
  } catch {
    return hostname;
  }
}

function clamp01(value: number): number {
  if (value < 0) {
    return 0;
  }
  if (value > 1) {
    return 1;
  }
  return value;
}

/** Map one raw API result into the UI `SearchResult` domain model. */
function mapResult(raw: unknown, maxScore: number): SearchResult | null {
  if (!isObject(raw)) {
    return null;
  }
  const docId = asString(raw.doc_id, '');
  const title = asString(raw.title, '');
  const url = asString(raw.canonical_url, '');
  if (docId.length === 0 || url.length === 0) {
    return null;
  }

  let hostname = 'unknown';
  try {
    hostname = new URL(url).hostname;
  } catch {
    hostname = asString(raw.source_id, 'unknown');
  }
  if (hostname.length === 0) {
    hostname = 'unknown';
  }

  const rawScore = asFiniteNumber(raw.score, 0);
  const normalizedScore = maxScore > 0 ? clamp01(rawScore / maxScore) : 0;

  const rawSignals = Array.isArray(raw.signals) ? raw.signals : [];
  const signals: SearchSignal[] = [];
  for (let i = 0; i < Math.min(rawSignals.length, MAX_RESULTS); i += 1) {
    const mapped = mapSignal(rawSignals[i]);
    if (mapped !== null) {
      signals.push(mapped);
    }
  }

  const lane = asString(raw.lane, '');
  const provenance = asString(raw.provenance, '');
  const liveOverlay = provenance === 'overlay' || lane === 'lexical_overlay';

  const tags: string[] = signals.map((s) => s.name);
  if (liveOverlay) {
    tags.push('live');
  }

  return {
    id: docId,
    title: title.length > 0 ? title : url,
    url,
    displayUrl: buildDisplayUrl(url, hostname),
    snippet: asString(raw.snippet, ''),
    category: 'all',
    score: normalizedScore,
    publishedAt: toIsoOrNow(raw.published_at ?? raw.fetched_at),
    sourceDomain: hostname,
    tags,
    signals,
    liveOverlay,
  };
}

/** Map a full `SearchResponse` into the UI result list, or `null` if malformed. */
export function mapSearchResponse(data: unknown): readonly SearchResult[] | null {
  if (!isSearchResponse(data)) {
    return null;
  }
  const rawResults = data.results;
  let maxScore = 0;
  for (let i = 0; i < rawResults.length; i += 1) {
    const r = rawResults[i];
    if (isObject(r)) {
      maxScore = Math.max(maxScore, asFiniteNumber(r.score, 0));
    }
  }
  const results: SearchResult[] = [];
  for (let i = 0; i < Math.min(rawResults.length, MAX_RESULTS); i += 1) {
    const mapped = mapResult(rawResults[i], maxScore);
    if (mapped !== null) {
      results.push(mapped);
    }
  }
  return Object.freeze(results);
}

export class HexxicaClient {
  private readonly baseUrl: string;

  public constructor(baseUrl: string = resolveBaseUrl()) {
    assertNonNull(baseUrl, 'baseUrl');
    assertBoundedString(baseUrl, 256, 'baseUrl');
    this.baseUrl = baseUrl.replace(/\/+$/, '');
  }

  public getBaseUrl(): string {
    return this.baseUrl;
  }

  /**
   * Probes the API's `GET /health` for liveness and index telemetry.
   */
  public async probeNodeHealth(): Promise<NodeHealthTelemetry> {
    const startTime = performance.now();
    const controller = new AbortController();
    const timeoutId = setTimeout(() => controller.abort(), NETWORK_TIMEOUT_MS);

    try {
      const response = await fetch(`${this.baseUrl}/health`, {
        method: 'GET',
        headers: { Accept: 'application/json' },
        signal: controller.signal,
        cache: 'no-cache',
      });
      const latency = Math.max(1, Math.round(performance.now() - startTime));

      if (!response.ok) {
        return {
          endpoint: this.baseUrl,
          status: 'DEGRADED',
          latencyMs: latency,
          lastCheckedIso: new Date().toISOString(),
          httpStatusCode: response.status,
          isSslActive: this.baseUrl.startsWith('https://'),
          diagnosticsMessage: `Health probe returned HTTP ${response.status}.`,
        };
      }

      const body: unknown = await response.json();
      const status: NodeStatusType = 'ONLINE';
      const generation = isObject(body) ? asFiniteNumber(body.generation, 0) : 0;

      return {
        endpoint: this.baseUrl,
        status,
        latencyMs: latency,
        lastCheckedIso: new Date().toISOString(),
        httpStatusCode: response.status,
        isSslActive: this.baseUrl.startsWith('https://'),
        diagnosticsMessage: `Local index generation ${generation} reachable.`,
      };
    } catch (err: unknown) {
      const latency = Math.round(performance.now() - startTime);
      const errorMessage = err instanceof Error ? err.message : 'Unknown communication error';

      return {
        endpoint: this.baseUrl,
        status: 'SIMULATION',
        latencyMs: latency,
        lastCheckedIso: new Date().toISOString(),
        httpStatusCode: 503,
        isSslActive: this.baseUrl.startsWith('https://'),
        diagnosticsMessage: `API offline or CORS-restricted (${errorMessage}). Active fallback buffer engaged.`,
      };
    } finally {
      clearTimeout(timeoutId);
    }
  }

  /**
   * POSTs the query to the API's `/search` and maps the `SearchResponse` into
   * the UI domain models. Returns `null` on any transport/parse failure so the
   * caller can fall back gracefully.
   */
  public async executeRemoteQuery(query: string): Promise<readonly SearchResult[] | null> {
    assertNonNull(query, 'query');
    if (query.trim().length === 0) {
      return [];
    }

    const controller = new AbortController();
    const timeoutId = setTimeout(() => controller.abort(), NETWORK_TIMEOUT_MS);

    try {
      const response = await fetch(`${this.baseUrl}/search`, {
        method: 'POST',
        headers: {
          'Content-Type': 'application/json',
          Accept: 'application/json',
        },
        body: JSON.stringify({ query }),
        signal: controller.signal,
      });

      if (!response.ok) {
        return null;
      }

      const rawJson: unknown = await response.json();
      return mapSearchResponse(rawJson);
    } catch {
      return null;
    } finally {
      clearTimeout(timeoutId);
    }
  }
}

export const hexxicaClientInstance = new HexxicaClient();
