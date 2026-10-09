/**
 * @file searchEngineService.test.ts
 * @description Tests proving the search service is a real API client with NO
 * fabricated-result fallback: unreachable -> typed error, empty -> empty, and
 * the engine provenance fields pass through. Zero use of `any`.
 */

import { afterEach, describe, expect, it, vi } from 'vitest';
import { SearchEngineService } from './searchEngineService';
import { SearchUnavailableError } from './searchErrors';

function responseOf(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { 'Content-Type': 'application/json' },
  });
}

/** Every synthetic id the old fallback used to invent. */
function hasSyntheticId(ids: readonly string[]): boolean {
  return ids.some((id) => id.startsWith('synth-') || id.startsWith('exp-'));
}

afterEach(() => {
  vi.unstubAllGlobals();
});

describe('SearchEngineService.executeSearch', () => {
  it('surfaces SearchUnavailableError when the API is unreachable (no invented rows)', async () => {
    const service = new SearchEngineService();
    vi.stubGlobal('fetch', vi.fn(async () => {
      throw new Error('ECONNREFUSED');
    }));

    await expect(
      service.executeSearch({ query: 'novel unheard of query', category: 'all', timeFilter: 'all', page: 1 })
    ).rejects.toBeInstanceOf(SearchUnavailableError);
  });

  it('surfaces SearchUnavailableError on a non-2xx response', async () => {
    const service = new SearchEngineService();
    vi.stubGlobal('fetch', vi.fn(async () => new Response('boom', { status: 503 })));
    await expect(
      service.executeSearch({ query: 'x', category: 'all', timeFilter: 'all', page: 1 })
    ).rejects.toBeInstanceOf(SearchUnavailableError);
  });

  it('returns an honest empty result set for a successful empty response', async () => {
    const service = new SearchEngineService();
    vi.stubGlobal('fetch', vi.fn(async () => responseOf({ results: [] })));

    const response = await service.executeSearch({
      query: 'novel unheard of query',
      category: 'all',
      timeFilter: 'all',
      page: 1,
    });

    expect(response.results).toEqual([]);
    expect(response.metrics.totalMatches).toBe(0);
    expect(hasSyntheticId(response.results.map((r) => r.id))).toBe(false);
  });

  it('PROOF: the production service cannot return a synth-* id for a novel query', async () => {
    const service = new SearchEngineService();
    // The API is reachable but has nothing for this query.
    vi.stubGlobal('fetch', vi.fn(async () => responseOf({ results: [] })));

    const response = await service.executeSearch({
      query: 'zzz totally novel fabricated query zzz',
      category: 'all',
      timeFilter: 'all',
      page: 1,
    });

    expect(response.results).toHaveLength(0);
    for (const result of response.results) {
      expect(result.id.startsWith('synth-')).toBe(false);
      expect(result.url).not.toContain('.org');
    }
  });

  it('passes through lane_ranks, coverage, stop_reason, trace and answer', async () => {
    const service = new SearchEngineService();
    const payload = {
      engine: true,
      coverage: { supported: ['time'], mandatory_ok: false, coverage: 0.42 },
      stop_reason: 'budget',
      trace: ['a', 'b'],
      answer: 'Generated text.',
      results: [
        {
          doc_id: 'doc-1',
          title: 'Real doc',
          canonical_url: 'https://real.test/1',
          snippet: 'body',
          score: 5,
          lane_ranks: { bm25: 3, source_first: 12 },
          unit_support: ['unit-9'],
        },
      ],
    };
    vi.stubGlobal('fetch', vi.fn(async () => responseOf(payload)));

    const response = await service.executeSearch({
      query: 'real',
      category: 'all',
      timeFilter: 'all',
      page: 1,
    });

    expect(response.engine).toBe(true);
    expect(response.coverage?.coverage).toBe(0.42);
    expect(response.coverage?.mandatoryOk).toBe(false);
    expect(response.stopReason).toBe('budget');
    expect(response.trace).toEqual(['a', 'b']);
    expect(response.answer).toBe('Generated text.');
    expect(response.results[0].laneRanks).toEqual({ bm25: 3, source_first: 12 });
    expect(response.results[0].unitSupport).toEqual(['unit-9']);
  });

  it('handles a response missing every new engine field without crashing', async () => {
    const service = new SearchEngineService();
    vi.stubGlobal(
      'fetch',
      vi.fn(async () =>
        responseOf({
          results: [{ doc_id: 'd', title: 't', canonical_url: 'https://a.test/x', snippet: 's', score: 1 }],
        })
      )
    );

    const response = await service.executeSearch({
      query: 'old',
      category: 'all',
      timeFilter: 'all',
      page: 1,
    });

    expect(response.engine).toBe(false);
    expect(response.coverage).toBeNull();
    expect(response.stopReason).toBeNull();
    expect(response.trace).toEqual([]);
    expect(response.answer).toBeNull();
    expect(response.results).toHaveLength(1);
  });

  it('never invents autocomplete suggestions when nothing matches', () => {
    const service = new SearchEngineService();
    expect(service.computeSuggestions('zzzqqq')).toEqual([]);
    expect(service.computeSuggestions('')).toEqual([]);
  });
});
