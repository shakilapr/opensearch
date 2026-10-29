/**
 * @file hexxicaClient.ts
 * @description Real HTTP client for the local search-api (`POST /search`, `GET /health`).
 * ISO/IEC 25010 Fault-Tolerance and NASA Rule 7 (Input & Return Validation).
 *
 * There is NO fabricated-result path here: when the API cannot answer, the client
 * raises a `SearchUnavailableError` so the caller renders an honest error state.
 * Zero use of `any`.
 */

import { assertBoundedString, assertNonNull } from '../core/assertions';
import {
  EngineCoverage,
  EngineMode,
  LlmAttemptView,
  NodeHealthTelemetry,
  NodeStatusType,
  SearchResult,
  SearchSignal,
  TokenUsageView,
} from '../domain/models';
import { SearchUnavailableError } from './searchErrors';

/** Default API base URL; override with the `VITE_API_BASE` env var. */
export const HEXXICA_DEFAULT_ENDPOINT = 'http://127.0.0.1:8080';
/**
 * How long to wait for `POST /search` before reporting the server unreachable.
 *
 * The engine's answer path calls a language model, so a request with `engine: true` is
 * model-bound: retrieval answers in ~0.1 s, the generated answer in ~3-20 s (and the server caps
 * the answer at its own timeout, returning the results regardless). A 6 s abort here reported
 * "Search unavailable" for a server that was working and about to answer, so the budget must
 * exceed the server's answer timeout — currently 40 s.
 */
const NETWORK_TIMEOUT_MS = 60000;
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
  /** Additive engine field: stable lowercase lane name -> rank. */
  readonly lane_ranks?: Readonly<Record<string, number>> | null;
  /** Additive engine field: evidence units supporting this result. */
  readonly unit_support?: readonly string[] | null;
}

/** The additive, optional engine envelope returned by `POST /search`. */
interface RawSearchResponse {
  readonly results: readonly RawSearchResult[];
  readonly engine?: boolean;
  readonly coverage?: unknown;
  readonly stop_reason?: unknown;
  readonly trace?: unknown;
  readonly answer?: unknown;
  /** Token counts of the answer's model call. */
  readonly usage?: unknown;
  /** The models tried for the answer, in order. */
  readonly llm_attempts?: unknown;
  /** Distinct documents matched before the display limit. */
  readonly total_matches?: unknown;
}

/** A mapped, engine-aware search response consumed by the service/UI. */
export interface SearchResponseData {
  readonly results: readonly SearchResult[];
  readonly engine: boolean;
  readonly coverage: EngineCoverage | null;
  readonly stopReason: string | null;
  readonly trace: readonly string[];
  readonly answer: string | null;
  /** Token counts of the answer's model call, `null` when the server reported none. */
  readonly usage: TokenUsageView | null;
  /** The models tried for the answer, in order. Empty when none was tried. */
  readonly llmAttempts: readonly LlmAttemptView[];
  /** Distinct documents matched before the display limit, `null` on an older server. */
  readonly totalMatches: number | null;
}

/** An empty but successful response (no network call, no invented rows). */
export const EMPTY_SEARCH_RESPONSE: SearchResponseData = Object.freeze({
  results: Object.freeze([]),
  engine: false,
  coverage: null,
  stopReason: null,
  trace: Object.freeze([]),
  answer: null,
  usage: null,
  llmAttempts: Object.freeze([]),
  totalMatches: null,
});

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null;
}

/** Validate an unknown payload as a `RawSearchResponse` without using `any`. */
function isSearchResponse(data: unknown): data is RawSearchResponse {
  if (!isObject(data)) {
    return false;
  }
  return Array.isArray(data.results);
}

/**
 * Remove highlight markup from a snippet.
 *
 * The API returns plain text (its snippet generator no longer emits HTML), but a response cached
 * before that fix — or served by an older index — can still carry Tantivy's `<b>`/`</b>` tags, and a
 * client that renders text shows them literally: a live result read "… DAST <b>winter</b> update
 * 2025 …". Stripping here means the UI can never display markup as content, whatever the server
 * sends. Highlighting is a presentation decision this UI simply does not make.
 */
function stripHighlightMarkup(text: string): string {
  return text.replace(/<\/?b>/gi, '');
}

function asString(value: unknown, fallback: string): string {
  return typeof value === 'string' ? value : fallback;
}

function asFiniteNumber(value: unknown, fallback: number): number {
  return typeof value === 'number' && Number.isFinite(value) ? value : fallback;
}

/** Read an unknown value as a bounded array of strings, dropping non-strings. */
function readStringArray(value: unknown): string[] {
  if (!Array.isArray(value)) {
    return [];
  }
  const source = value as readonly unknown[];
  const out: string[] = [];
  for (let i = 0; i < source.length; i += 1) {
    const item: unknown = source[i];
    if (typeof item === 'string') {
      out.push(item);
    }
  }
  return out;
}

/** Read the optional `lane_ranks` map, tolerating absent/null/malformed input. */
function readLaneRanks(value: unknown): Readonly<Record<string, number>> | undefined {
  if (!isObject(value)) {
    return undefined;
  }
  const out: Record<string, number> = {};
  let hasAny = false;
  const keys = Object.keys(value);
  for (let i = 0; i < keys.length; i += 1) {
    const key = keys[i];
    if (key !== undefined) {
      const raw = value[key];
      if (typeof raw === 'number' && Number.isFinite(raw)) {
        out[key] = raw;
        hasAny = true;
      }
    }
  }
  return hasAny ? Object.freeze(out) : undefined;
}

/** Read the optional `unit_support` array, tolerating absent/null input. */
function readUnitSupport(value: unknown): readonly string[] | undefined {
  const items = readStringArray(value);
  return items.length > 0 ? Object.freeze(items) : undefined;
}

/**
 * Read the optional `usage` object: the answer's token counts. Returns `null` when the server
 * reported none, so an older server renders no token line rather than zeros.
 */
function readUsage(value: unknown): TokenUsageView | null {
  if (!isObject(value)) {
    return null;
  }
  const input = asFiniteNumber(value.input, -1);
  const output = asFiniteNumber(value.output, -1);
  const cached = asFiniteNumber(value.cached, -1);
  if (input < 0 || output < 0 || cached < 0) {
    return null;
  }
  return Object.freeze({ input, output, cached });
}

/**
 * Read the optional `llm_attempts` array: which models were tried, which failed and which
 * answered. Entries missing a provider or model are dropped rather than rendered as blanks.
 */
function readAttempts(value: unknown): readonly LlmAttemptView[] {
  if (!Array.isArray(value)) {
    return Object.freeze([]);
  }
  const attempts: LlmAttemptView[] = [];
  for (let i = 0; i < value.length; i += 1) {
    const raw = value[i];
    if (!isObject(raw)) {
      continue;
    }
    const provider = typeof raw.provider === 'string' ? raw.provider : '';
    const model = typeof raw.model === 'string' ? raw.model : '';
    if (provider.length === 0 || model.length === 0) {
      continue;
    }
    const error = typeof raw.error === 'string' && raw.error.length > 0 ? raw.error : null;
    attempts.push(
      Object.freeze({ provider, model, ok: raw.ok === true, error })
    );
  }
  return Object.freeze(attempts);
}

/**
 * Read the optional `coverage` object defensively. Returns `null` when no
 * recognised field is present, so older servers render nothing.
 */
function readCoverage(value: unknown): EngineCoverage | null {
  if (!isObject(value)) {
    return null;
  }
  const supported = readStringArray(value.supported ?? value.supported_flags ?? value.flags);
  const unsupported = readStringArray(value.unsupported);
  const mandatoryRaw = value.mandatory_ok ?? value.mandatoryOk;
  const coverageRaw = value.coverage;
  const coverage =
    typeof coverageRaw === 'number' && Number.isFinite(coverageRaw) ? coverageRaw : null;
  const hasMandatory = typeof mandatoryRaw === 'boolean';
  if (supported.length === 0 && unsupported.length === 0 && !hasMandatory && coverage === null) {
    return null;
  }
  return {
    supported: Object.freeze(supported),
    unsupported: Object.freeze(unsupported),
    mandatoryOk: hasMandatory ? mandatoryRaw === true : false,
    coverage,
  };
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
    snippet: stripHighlightMarkup(asString(raw.snippet, '')),
    category: 'all',
    score: normalizedScore,
    publishedAt: toIsoOrNow(raw.published_at ?? raw.fetched_at),
    sourceDomain: hostname,
    tags,
    signals,
    liveOverlay,
    laneRanks: readLaneRanks(raw.lane_ranks),
    unitSupport: readUnitSupport(raw.unit_support),
  };
}

/** Map the `results` array of a response, or `null` if the payload is malformed. */
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

/**
 * Map a full response (results + optional engine envelope) into the UI shape.
 * Returns `null` for a malformed payload. Absent engine fields map to neutral
 * defaults so an older server renders without crashing.
 */
export function mapSearchResponseData(data: unknown): SearchResponseData | null {
  if (!isSearchResponse(data)) {
    return null;
  }
  const results = mapSearchResponse(data) ?? Object.freeze([]);
  const stopReasonRaw = data.stop_reason;
  const answerRaw = data.answer;
  const answer = typeof answerRaw === 'string' && answerRaw.length > 0 ? answerRaw : null;
  return {
    results,
    engine: data.engine === true,
    coverage: readCoverage(data.coverage),
    stopReason: typeof stopReasonRaw === 'string' ? stopReasonRaw : null,
    trace: Object.freeze(readStringArray(data.trace)),
    answer,
    usage: readUsage(data.usage),
    llmAttempts: readAttempts(data.llm_attempts),
    totalMatches:
      typeof data.total_matches === 'number' && Number.isFinite(data.total_matches)
        ? data.total_matches
        : null,
  };
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
        diagnosticsMessage: `Search API offline or CORS-restricted (${errorMessage}). Searches will fail until it is reachable.`,
      };
    } finally {
      clearTimeout(timeoutId);
    }
  }

  /**
   * POSTs the query to the API's `/search` with `engine: true` and a `mode`,
   * and maps the response into the UI domain models. Throws a typed
   * `SearchUnavailableError` on any transport/status/parse failure — it never
   * returns fabricated results.
   */
  public async executeRemoteQuery(
    query: string,
    mode: EngineMode = 'auto'
  ): Promise<SearchResponseData> {
    assertNonNull(query, 'query');
    if (query.trim().length === 0) {
      return EMPTY_SEARCH_RESPONSE;
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
        body: JSON.stringify({ query, engine: true, mode }),
        signal: controller.signal,
      });

      if (!response.ok) {
        throw new SearchUnavailableError(this.baseUrl, 'http_error', response.status);
      }

      const rawJson: unknown = await response.json();
      const mapped = mapSearchResponseData(rawJson);
      if (mapped === null) {
        throw new SearchUnavailableError(this.baseUrl, 'malformed_response', response.status);
      }
      return mapped;
    } catch (err: unknown) {
      if (err instanceof SearchUnavailableError) {
        throw err;
      }
      throw new SearchUnavailableError(this.baseUrl, 'unreachable');
    } finally {
      clearTimeout(timeoutId);
    }
  }
}

export const hexxicaClientInstance = new HexxicaClient();
