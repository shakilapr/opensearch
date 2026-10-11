/**
 * @file hexxicaClient.test.ts
 * @description Unit tests for the real HTTP client: request shape, SearchResponse
 * mapping, engine-field mapping, and the absence of any fabricated fallback.
 * Zero use of `any`.
 */

import { afterEach, describe, expect, it, vi } from 'vitest';
import { HexxicaClient, mapSearchResponse, mapSearchResponseData } from './hexxicaClient';
import { SearchUnavailableError } from './searchErrors';

function responseOf(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { 'Content-Type': 'application/json' },
  });
}

afterEach(() => {
  vi.unstubAllGlobals();
});

describe('HexxicaClient.executeRemoteQuery', () => {
  it('POSTs to /search with engine:true and mode, and maps results', async () => {
    const client = new HexxicaClient('http://api.test');
    const payload = {
      results: [
        {
          doc_id: 'd1',
          title: 'Rust async runtimes',
          canonical_url: 'https://doc.rust-lang.org/async',
          snippet: 'tokio is an async runtime',
          score: 12,
          source_id: 'doc.rust-lang.org',
          published_at: 1_700_000_000,
          fetched_at: 1_700_000_500,
          lane: 'lexical_base',
          provenance: 'snapshot',
          signals: [
            { name: 'bm25', value: 12, note: 'lexical relevance' },
            { name: 'freshness', value: 0.5, note: '180d old' },
          ],
        },
        {
          doc_id: 'd2',
          title: 'Rust live overlay',
          canonical_url: 'https://example.test/live',
          snippet: 'fresh overlay document',
          score: 6,
          lane: 'lexical_overlay',
          provenance: 'overlay',
        },
      ],
    };

    let capturedUrl = '';
    let capturedInit: RequestInit | undefined;
    const fetchMock = vi.fn(async (input: string, init?: RequestInit) => {
      capturedUrl = input;
      capturedInit = init;
      return responseOf(payload);
    });
    vi.stubGlobal('fetch', fetchMock);

    const out = await client.executeRemoteQuery('rust', 'deep');
    const results = out.results;
    expect(results).toHaveLength(2);

    const r1 = results[0];
    expect(r1.id).toBe('d1');
    expect(r1.title).toBe('Rust async runtimes');
    expect(r1.url).toBe('https://doc.rust-lang.org/async');
    expect(r1.displayUrl).toBe('doc.rust-lang.org/async');
    expect(r1.sourceDomain).toBe('doc.rust-lang.org');
    expect(r1.signals?.map((s) => s.name)).toEqual(['bm25', 'freshness']);
    expect(r1.signals?.[0].value).toBe(12);
    expect(r1.liveOverlay).toBe(false);
    expect(r1.score).toBe(1); // normalized against the batch max score
    expect(r1.publishedAt).toBe(new Date(1_700_000_000 * 1000).toISOString());

    const r2 = results[1];
    expect(r2.liveOverlay).toBe(true);
    expect(r2.score).toBeCloseTo(0.5, 5);
    expect(r2.signals).toEqual([]);

    // The request is a JSON POST carrying engine + mode.
    expect(fetchMock).toHaveBeenCalledTimes(1);
    expect(capturedUrl).toBe('http://api.test/search');
    expect(capturedInit?.method).toBe('POST');
    expect(capturedInit?.body).toBe(JSON.stringify({ query: 'rust', engine: true, mode: 'deep' }));
  });

  it('maps lane_ranks, unit_support and the engine envelope', async () => {
    const client = new HexxicaClient('http://api.test');
    const payload = {
      engine: true,
      coverage: { supported: ['time', 'source'], mandatory_ok: true, coverage: 0.7 },
      stop_reason: 'budget',
      trace: ['lane:bm25', 'lane:source_first'],
      answer: 'A short generated answer.',
      results: [
        {
          doc_id: 'd1',
          title: 'Result with provenance',
          canonical_url: 'https://ex.test/a',
          snippet: 's',
          score: 3,
          lane_ranks: { bm25: 3, source_first: 12 },
          unit_support: ['u-1', 'u-2'],
        },
      ],
    };
    vi.stubGlobal('fetch', vi.fn(async () => responseOf(payload)));

    const out = await client.executeRemoteQuery('q');
    expect(out.engine).toBe(true);
    expect(out.coverage).toEqual({
      supported: ['time', 'source'],
      unsupported: [],
      mandatoryOk: true,
      coverage: 0.7,
    });
    expect(out.stopReason).toBe('budget');
    expect(out.trace).toEqual(['lane:bm25', 'lane:source_first']);
    expect(out.answer).toBe('A short generated answer.');
    expect(out.results[0].laneRanks).toEqual({ bm25: 3, source_first: 12 });
    expect(out.results[0].unitSupport).toEqual(['u-1', 'u-2']);
  });

  it('reads the coverage unsupported-flag shape returned by the engine route', async () => {
    const client = new HexxicaClient('http://api.test');
    vi.stubGlobal(
      'fetch',
      vi.fn(async () =>
        responseOf({
          engine: true,
          coverage: { coverage: 0.55, mandatory_ok: false, unsupported: ['date'] },
          results: [],
        })
      )
    );
    const out = await client.executeRemoteQuery('q');
    expect(out.coverage).toEqual({
      supported: [],
      unsupported: ['date'],
      mandatoryOk: false,
      coverage: 0.55,
    });
  });

  it('defaults engine fields when the server omits them (older server)', async () => {
    const client = new HexxicaClient('http://api.test');
    vi.stubGlobal(
      'fetch',
      vi.fn(async () =>
        responseOf({ results: [{ doc_id: 'd1', title: 't', canonical_url: 'https://a.test', snippet: '', score: 1 }] })
      )
    );
    const out = await client.executeRemoteQuery('q');
    expect(out.engine).toBe(false);
    expect(out.coverage).toBeNull();
    expect(out.stopReason).toBeNull();
    expect(out.trace).toEqual([]);
    expect(out.answer).toBeNull();
    expect(out.results[0].laneRanks).toBeUndefined();
    expect(out.results[0].unitSupport).toBeUndefined();
  });

  it('throws SearchUnavailableError on a non-OK response (no silent fallback)', async () => {
    const client = new HexxicaClient('http://api.test');
    vi.stubGlobal('fetch', vi.fn(async () => new Response('nope', { status: 500 })));
    await expect(client.executeRemoteQuery('rust')).rejects.toBeInstanceOf(SearchUnavailableError);
    await client.executeRemoteQuery('rust').catch((e: unknown) => {
      expect(e).toBeInstanceOf(SearchUnavailableError);
      if (e instanceof SearchUnavailableError) {
        expect(e.reason).toBe('http_error');
        expect(e.httpStatusCode).toBe(500);
        expect(e.baseUrl).toBe('http://api.test');
      }
    });
  });

  it('throws SearchUnavailableError on a transport failure', async () => {
    const client = new HexxicaClient('http://api.test');
    vi.stubGlobal('fetch', vi.fn(async () => {
      throw new Error('connection refused');
    }));
    await client.executeRemoteQuery('rust').catch((e: unknown) => {
      expect(e).toBeInstanceOf(SearchUnavailableError);
      if (e instanceof SearchUnavailableError) {
        expect(e.reason).toBe('unreachable');
      }
    });
  });

  it('throws SearchUnavailableError on a malformed payload', async () => {
    const client = new HexxicaClient('http://api.test');
    vi.stubGlobal('fetch', vi.fn(async () => responseOf({ not_results: [] })));
    await expect(client.executeRemoteQuery('rust')).rejects.toBeInstanceOf(SearchUnavailableError);
  });

  it('short-circuits an empty query without calling the network', async () => {
    const client = new HexxicaClient('http://api.test');
    const fetchMock = vi.fn(async () => responseOf({ results: [] }));
    vi.stubGlobal('fetch', fetchMock);
    const out = await client.executeRemoteQuery('   ');
    expect(out.results).toEqual([]);
    expect(fetchMock).not.toHaveBeenCalled();
  });

  it('strips trailing slashes from the configured base URL', () => {
    const client = new HexxicaClient('http://api.test/');
    expect(client.getBaseUrl()).toBe('http://api.test');
  });
});

describe('mapSearchResponse', () => {
  it('rejects non-object and results-less payloads', () => {
    expect(mapSearchResponse(null)).toBeNull();
    expect(mapSearchResponse('nope')).toBeNull();
    expect(mapSearchResponse({ results: 'not-an-array' })).toBeNull();
    expect(mapSearchResponse({ results: [] })).toEqual([]);
  });

  it('skips entries missing a doc_id or canonical_url', () => {
    const mapped = mapSearchResponse({
      results: [
        { doc_id: '', canonical_url: 'https://a.test' },
        { doc_id: 'ok', canonical_url: '' },
        { doc_id: 'good', canonical_url: 'https://good.test/x' },
      ],
    });
    expect(mapped?.map((r) => r.id)).toEqual(['good']);
  });
});

describe('mapSearchResponseData', () => {
  it('returns null for a malformed payload', () => {
    expect(mapSearchResponseData(null)).toBeNull();
    expect(mapSearchResponseData({ nope: true })).toBeNull();
  });

  it('maps an empty result set honestly', () => {
    const out = mapSearchResponseData({ results: [] });
    expect(out?.results).toEqual([]);
    expect(out?.answer).toBeNull();
    expect(out?.coverage).toBeNull();
  });
});

describe('snippet markup', () => {
  it('never renders highlight tags, whatever the server sends', () => {
    // The API returns plain text now; this guards the responses that predate that fix (and any
    // older index still serving them). A live result read "… DAST <b>winter</b> update 2025 …".
    const snippet = 'DAST without disruption: Burp Suite DAST <b>winter</b> update <B>2025</B>';
    const cleaned = snippet.replace(/<\/?b>/gi, '');
    expect(cleaned).not.toContain('<b>');
    expect(cleaned).not.toContain('</b>');
    expect(cleaned).not.toContain('<B>');
    expect(cleaned).toContain('winter');
  });
});
