/**
 * @file SearchResultsView.test.tsx
 * @description Rendering tests for the results stream: honest error state, empty
 * state, full engine provenance rendering, and graceful handling of an older
 * server that omits every new field. Zero use of `any`.
 */

import React from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { describe, expect, it } from 'vitest';
import { SearchResult } from '../domain/models';
import { SearchUnavailableError } from '../services/searchErrors';
import { SearchResultsView } from './SearchResultsView';

function resultOf(overrides: Partial<SearchResult> = {}): SearchResult {
  return {
    id: 'doc-1',
    title: 'A real indexed document',
    url: 'https://real.test/doc',
    displayUrl: 'real.test/doc',
    snippet: 'snippet body',
    category: 'all',
    score: 1,
    publishedAt: '2026-10-01T00:00:00Z',
    sourceDomain: 'real.test',
    tags: [],
    ...overrides,
  };
}

type ViewProps = React.ComponentProps<typeof SearchResultsView>;

const BASE_PROPS: ViewProps = {
  results: [],
  isLoading: false,
  category: 'all',
  query: 'real',
  searchError: null,
  baseUrl: 'http://127.0.0.1:8080',
  engine: false,
  coverage: null,
  stopReason: null,
  answer: null,
  llmAttempts: [],
  trace: [],
  onRetry: () => undefined,
};

function render(overrides: Partial<ViewProps>): string {
  const merged = { ...BASE_PROPS, ...overrides } as ViewProps;
  return renderToStaticMarkup(<SearchResultsView {...merged} />);
}

describe('SearchResultsView', () => {
  it('renders an actionable unavailable state when the API is unreachable', () => {
    const html = render({
      searchError: new SearchUnavailableError('http://127.0.0.1:8080', 'unreachable'),
    });
    expect(html).toContain('not reachable at the configured base URL');
    expect(html).toContain('http://127.0.0.1:8080');
    expect(html).toContain('Retry');
    // No fabricated rows leak in.
    expect(html).not.toContain('synth-');
  });

  it('renders an honest empty state for a successful empty response', () => {
    const html = render({ results: [] });
    expect(html).toContain('No results for this query');
    expect(html).not.toContain('synth-');
  });

  it('renders lane_ranks, coverage/stop_reason, and the answer panel together', () => {
    const html = render({
      results: [
        resultOf({
          laneRanks: { bm25: 3, source_first: 12 },
          unitSupport: ['unit-9'],
        }),
      ],
      engine: true,
      coverage: { supported: ['time', 'source'], unsupported: [], mandatoryOk: true, coverage: 0.7 },
      stopReason: 'budget',
      answer: 'Generated from the indexed evidence.',
    });

    // Per-result provenance.
    expect(html).toContain('Why this result?');
    expect(html).toContain('bm25 #3');
    expect(html).toContain('source_first #12');
    expect(html).toContain('unit-9');
    // Coverage / stop-reason strip.
    expect(html).toContain('coverage 0.70');
    expect(html).toContain('mandatory predicates satisfied');
    expect(html).toContain('stopped: budget');
    // Answer panel with label + disclaimer.
    expect(html).toContain('Generated answer from indexed evidence');
    expect(html).toContain('Generated from the indexed evidence.');
    expect(html).toContain('model-generated and may be incomplete');
  });

  it('renders an older-server response (all new fields absent) without crashing', () => {
    const html = render({ results: [resultOf()] });
    expect(html).toContain('A real indexed document');
    expect(html).not.toContain('Why this result?');
    expect(html).not.toContain('Generated answer');
  });

  it('names every model tried, with the error of the failed ones and the one that answered', () => {
    const html = render({
      results: [resultOf()],
      engine: true,
      llmAttempts: [
        {
          provider: 'openrouter',
          model: 'google/gemma-4-31b-it:free',
          ok: false,
          error: 'HTTP 401 Unauthorized',
        },
        { provider: 'crowbot', model: 'deepseek-v4.1-flash', ok: true, error: null },
      ],
      trace: ['plan: mode=Fast lanes=[Bm25] top_k=10 variants=2 rerank=30 refresh=0'],
    });

    // What failed, and why — not just "no answer".
    expect(html).toContain('models tried:');
    expect(html).toContain('openrouter/google/gemma-4-31b-it:free (failed: HTTP 401 Unauthorized)');
    expect(html).toContain('crowbot/deepseek-v4.1-flash (answered)');
    // The search's own plan data.
    expect(html).toContain('plan: mode=Fast');
  });

  it('shows no model line when nothing was tried', () => {
    const html = render({ results: [resultOf()], engine: true, llmAttempts: [] });
    expect(html).not.toContain('models tried:');
  });
});
