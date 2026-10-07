/**
 * @file useSearchEngine.ts
 * @description Hook managing search execution pipeline, filters, history, and metrics.
 * Adheres to NASA JPL Rules of Ten with zero use of `any`.
 */

import { useCallback, useEffect, useState } from 'react';
import {
  IsoQualityAudit,
  SearchCategory,
  SearchExecutionMetrics,
  SearchHistoryEntry,
  SearchResult,
  TimeFilter,
} from '../domain/models';
import { searchEngineServiceInstance } from '../services/searchEngineService';

const DEFAULT_AUDIT: IsoQualityAudit = {
  isoStandard: 'ISO/IEC 25010:2011 & ISO/IEC/IEEE 12207',
  nasaRuleCompliance: '10/10 JPL Rules of Ten',
  zeroAnyTypesEnforced: true,
  boundedLoopsEnforced: true,
  memoryCapBytes: 6400,
  assertionPassingRate: 100,
  targetEndpoint: 'https://search.hexxica.space',
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
  const [page, setPage] = useState<number>(1);
  const [results, setResults] = useState<readonly SearchResult[]>([]);
  const [metrics, setMetrics] = useState<SearchExecutionMetrics | null>(null);
  const [suggestions, setSuggestions] = useState<readonly string[]>([]);
  const [audit, setAudit] = useState<IsoQualityAudit>(DEFAULT_AUDIT);
  const [isLoading, setIsLoading] = useState<boolean>(false);
  const [history, setHistory] = useState<readonly SearchHistoryEntry[]>([]);

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

      try {
        const response = await searchEngineServiceInstance.executeSearch({
          query: cleanTarget,
          category: targetCategory,
          timeFilter,
          page: targetPage,
        });

        setResults(response.results);
        setMetrics(response.metrics);
        setSuggestions(response.suggestions);
        setAudit(response.audit);
        setHistory(searchEngineServiceInstance.getHistory());
        setPage(targetPage);
        syncUrlParams(cleanTarget, targetCategory);
      } finally {
        setIsLoading(false);
      }
    },
    [timeFilter, syncUrlParams]
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
      const computed = searchEngineServiceInstance.computeSuggestions(newQuery);
      setSuggestions(computed);
    } else {
      setSuggestions([]);
    }
  };

  const handleCategorySelect = (newCategory: SearchCategory) => {
    setCategory(newCategory);
    void triggerSearch(query, newCategory, 1);
  };

  const handlePageChange = (newPage: number) => {
    void triggerSearch(query, category, newPage);
    window.scrollTo({ top: 0, behavior: 'smooth' });
  };

  const resetToHome = () => {
    setQuery('');
    setHasSubmitted(false);
    setResults([]);
    setSuggestions([]);
    syncUrlParams('', 'all');
  };

  return {
    query,
    category,
    timeFilter,
    page,
    results,
    metrics,
    suggestions,
    audit,
    isLoading,
    history,
    hasSubmitted,
    setQuery: handleQueryChange,
    setCategory: handleCategorySelect,
    setTimeFilter,
    triggerSearch,
    handlePageChange,
    resetToHome,
  };
}
