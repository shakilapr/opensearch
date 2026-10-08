/**
 * @file hexxicaClient.test.ts
 * @description Unit tests for the real HTTP client's SearchResponse mapping.
 * Zero use of `any`.
 */

import { afterEach, describe, expect, it, vi } from 'vitest';
import { HexxicaClient, mapSearchResponse } from './hexxicaClient';

function responseOf(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { 'Content-Type': 'application/json' },
  });
}

afterEach(() => {
  vi.unstubAllGlobals();
});

describe('HexxicaClient.executeRemoteQuery mapping', () => {
  it('POSTs to /search and maps the SearchResponse into UI models', async () => {
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

    const out = await client.executeRemoteQuery('rust');
    expect(out).not.toBeNull();
    const results = out ?? [];
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

    // The request is a JSON POST to <base>/search.
    expect(fetchMock).toHaveBeenCalledTimes(1);
    expect(capturedUrl).toBe('http://api.test/search');
    expect(capturedInit?.method).toBe('POST');
    expect(capturedInit?.body).toBe(JSON.stringify({ query: 'rust' }));
  });

  it('returns null on a non-OK response so the caller can fall back', async () => {
    const client = new HexxicaClient('http://api.test');
    vi.stubGlobal('fetch', vi.fn(async () => new Response('nope', { status: 500 })));
    await expect(client.executeRemoteQuery('rust')).resolves.toBeNull();
  });

  it('returns null on a malformed payload', async () => {
    const client = new HexxicaClient('http://api.test');
    vi.stubGlobal('fetch', vi.fn(async () => responseOf({ not_results: [] })));
    await expect(client.executeRemoteQuery('rust')).resolves.toBeNull();
  });

  it('short-circuits an empty query without calling the network', async () => {
    const client = new HexxicaClient('http://api.test');
    const fetchMock = vi.fn(async () => responseOf({ results: [] }));
    vi.stubGlobal('fetch', fetchMock);
    await expect(client.executeRemoteQuery('   ')).resolves.toEqual([]);
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
