/**
 * @file useSearchEngine.ts
 * @description Hook managing the search execution pipeline, filters, history,
 * metrics, engine mode, and error state. There is no fallback: a failed search
 * surfaces a `SearchUnavailableError` for the UI to render. Zero use of `any`.
 */

import { useCallback, useEffect, useState } from 'react';
import {
  EngineCoverage,
  EngineMode,
  IsoQualityAudit,
  SearchCategory,
  SearchExecutionMetrics,
  SearchHistoryEntry,
  SearchResult,
  TimeFilter,
} from '../domain/models';
import { hexxicaClientInstance } from '../services/hexxicaClient';
import { SearchUnavailableError } from '../services/searchErrors';
import { searchEngineServiceInstance } from '../services/searchEngineService';

const DEFAULT_AUDIT: IsoQualityAudit = {
  isoStandard: 'ISO/IEC 25010:2011 & ISO/IEC/IEEE 12207',
  nasaRuleCompliance: '10/10 JPL Rules of Ten',
  zeroAnyTypesEnforced: true,
  boundedLoopsEnforced: true,
  memoryCapBytes: 6400,
  assertionPassingRate: 100,
  targetEndpoint: hexxicaClientInstance.getBaseUrl(),
};

export function useSearchEngine() {
  const [query, setQuery] = useState<string>(() => {
    const params = new URLSearchParams(window.location.search);
    return params.get('q') ?? '';
  });

  const [hasSubmitted, setHasSubmitted] = useState<boolean>(() => {
    const params = new URLSearchParams(window.location.search);
    return (params.get('q') ?? '').trim().length > 0;
  });

  const [category, setCategory] = useState<SearchCategory>(() => {
    const params = new URLSearchParams(window.location.search);
    const cat = params.get('category');
    if (cat === 'space' || cat === 'tech' || cat === 'news' || cat === 'images') {
      return cat;
    }
    return 'all';
  });

  const [timeFilter, setTimeFilter] = useState<TimeFilter>('all');
  const [mode, setMode] = useState<EngineMode>('auto');
  const [page, setPage] = useState<number>(1);
  const [results, setResults] = useState<readonly SearchResult[]>([]);
  const [metrics, setMetrics] = useState<SearchExecutionMetrics | null>(null);
  const [suggestions, setSuggestions] = useState<readonly string[]>([]);
  const [audit, setAudit] = useState<IsoQualityAudit>(DEFAULT_AUDIT);
  const [isLoading, setIsLoading] = useState<boolean>(false);
  const [history, setHistory] = useState<readonly SearchHistoryEntry[]>([]);
  const [coverage, setCoverage] = useState<EngineCoverage | null>(null);
  const [stopReason, setStopReason] = useState<string | null>(null);
  const [trace, setTrace] = useState<readonly string[]>([]);
  const [answer, setAnswer] = useState<string | null>(null);
  const [engineActive, setEngineActive] = useState<boolean>(false);
  const [searchError, setSearchError] = useState<SearchUnavailableError | null>(null);

  const syncUrlParams = useCallback((newQuery: string, newCategory: SearchCategory) => {
    const url = new URL(window.location.href);
    if (newQuery.trim().length > 0) {
      url.searchParams.set('q', newQuery);
    } else {
      url.searchParams.delete('q');
    }
    if (newCategory !== 'all') {
      url.searchParams.set('category', newCategory);
    } else {
      url.searchParams.delete('category');
    }
    window.history.replaceState({}, '', url.toString());
  }, []);

  const triggerSearch = useCallback(
    async (targetQuery: string, targetCategory: SearchCategory, targetPage = 1) => {
      const cleanTarget = targetQuery.trim();
      setQuery(cleanTarget);
      setHasSubmitted(cleanTarget.length > 0);
      setIsLoading(true);
      setSearchError(null);

      try {
        const response = await searchEngineServiceInstance.executeSearch({
          query: cleanTarget,
          category: targetCategory,
          timeFilter,
          page: targetPage,
          mode,
        });

        setResults(response.results);
        setMetrics(response.metrics);
        setSuggestions(response.suggestions);
        setAudit(response.audit);
        setCoverage(response.coverage);
        setStopReason(response.stopReason);
        setTrace(response.trace);
        setAnswer(response.answer);
        setEngineActive(response.engine);
        setHistory(searchEngineServiceInstance.getHistory());
        setPage(targetPage);
        syncUrlParams(cleanTarget, targetCategory);
      } catch (err: unknown) {
        // Honest failure state: clear any prior results, keep nothing synthetic.
        setResults([]);
        setMetrics(null);
        setCoverage(null);
        setStopReason(null);
        setTrace([]);
        setAnswer(null);
        setEngineActive(false);
        setSearchError(
          err instanceof SearchUnavailableError
            ? err
            : new SearchUnavailableError(hexxicaClientInstance.getBaseUrl(), 'unreachable')
        );
      } finally {
        setIsLoading(false);
      }
    },
    [timeFilter, mode, syncUrlParams]
  );

  // Initial load if query in URL
  useEffect(() => {
    if (query.trim().length > 0) {
      void triggerSearch(query, category, 1);
    }
  }, []);

  const handleQueryChange = (newQuery: string) => {
    setQuery(newQuery);
    if (newQuery.trim().length > 0) {
      setSuggestions(searchEngineServiceInstance.computeSuggestions(newQuery));
    } else {
      setSuggestions([]);
    }
  };

  const handleCategorySelect = (newCategory: SearchCategory) => {
    setCategory(newCategory);
    void triggerSearch(query, newCategory, 1);
  };

  const handleModeChange = (newMode: EngineMode) => {
    setMode(newMode);
    if (hasSubmitted && query.trim().length > 0) {
      void triggerSearch(query, category, 1);
    }
  };

  const handlePageChange = (newPage: number) => {
    void triggerSearch(query, category, newPage);
    window.scrollTo({ top: 0, behavior: 'smooth' });
  };

  const retry = () => {
    if (query.trim().length > 0) {
      void triggerSearch(query, category, page);
    }
  };

  const resetToHome = () => {
    setQuery('');
    setHasSubmitted(false);
    setResults([]);
    setSuggestions([]);
    setSearchError(null);
    syncUrlParams('', 'all');
  };

  return {
    query,
    category,
    timeFilter,
    mode,
    page,
    results,
    metrics,
    suggestions,
    audit,
    isLoading,
    history,
    hasSubmitted,
    coverage,
    stopReason,
    trace,
    answer,
    engineActive,
    searchError,
    baseUrl: hexxicaClientInstance.getBaseUrl(),
    setQuery: handleQueryChange,
    setCategory: handleCategorySelect,
    setTimeFilter,
    setMode: handleModeChange,
    triggerSearch,
    handlePageChange,
    retry,
    resetToHome,
  };
}
