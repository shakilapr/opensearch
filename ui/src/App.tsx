/**
 * @file App.tsx
 * @description Ultra-Lightweight Minimalist Search Engine Shell.
 * Adheres to NASA JPL Rules of Ten and ISO/IEC 25010 Quality Model.
 * Zero emojis, SVG vector iconography, and zero use of `any`.
 */

import React, { useState } from 'react';
import { SearchFooter } from './components/SearchFooter';
import { SearchHeader } from './components/SearchHeader';
import { SearchHome } from './components/SearchHome';
import { SearchPagination } from './components/SearchPagination';
import { SearchResultsView } from './components/SearchResultsView';
import { TelemetryModal } from './components/TelemetryModal';
import { useHexxicaTelemetry } from './hooks/useHexxicaTelemetry';
import { useSearchEngine } from './hooks/useSearchEngine';

export const App: React.FC = () => {
  const [isDarkMode, setIsDarkMode] = useState<boolean>(true);
  const [isTelemetryOpen, setIsTelemetryOpen] = useState<boolean>(false);

  const { telemetry, refreshTelemetry } = useHexxicaTelemetry();
  const {
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
    hasSubmitted,
    coverage,
    stopReason,
    answer,
    engineActive,
    usage,
    llmAttempts,
    engineTotalMatches,
    trace,
    searchError,
    baseUrl,
    setQuery,
    setCategory,
    setTimeFilter,
    setMode,
    triggerSearch,
    handlePageChange,
    retry,
    resetToHome,
  } = useSearchEngine();

  const toggleTheme = () => {
    setIsDarkMode((prev) => {
      const next = !prev;
      document.documentElement.setAttribute('data-theme', next ? 'dark' : 'light');
      return next;
    });
  };

  const isHome = !hasSubmitted;
  const showPagination = !isLoading && searchError === null && results.length > 0;

  /**
   * Compact token counts: 1981 -> "2.0k", 402 -> "402". Reported figures only — the server
   * measures them, this only shortens them for the header.
   */
  const formatTokens = (n: number): string => {
    if (n >= 1_000_000) {
      return `${(n / 1_000_000).toFixed(1)}M`;
    }
    if (n >= 1_000) {
      return `${(n / 1_000).toFixed(1)}k`;
    }
    return `${n}`;
  };

  // The stats line answers three questions at once: how many documents matched, which model
  // answered, and what that cost. Each part comes from the server; nothing is estimated, and a
  // part the server did not report is simply absent.
  const statsParts: string[] = [];
  if (metrics !== null) {
    const total = engineTotalMatches ?? metrics.totalMatches;
    statsParts.push(`${total} result${total === 1 ? '' : 's'} (${metrics.executionTimeMs}ms)`);
  }
  const answering = llmAttempts.find((attempt) => attempt.ok) ?? null;
  if (answering !== null) {
    statsParts.push(`model: ${answering.provider}/${answering.model}`);
  }
  if (usage !== null) {
    statsParts.push(
      `tokens in: ${formatTokens(usage.input)}, out: ${formatTokens(usage.output)}, cached: ${formatTokens(usage.cached)}`
    );
  }
  statsParts.push(`Node: ${baseUrl}`);
  const statsText = statsParts.join(' • ');

  return (
    <div className="hx-app" data-theme={isDarkMode ? 'dark' : 'light'}>
      {isHome ? (
        <SearchHome
          query={query}
          suggestions={suggestions}
          telemetry={telemetry}
          isDarkMode={isDarkMode}
          mode={mode}
          onModeChange={setMode}
          onQueryChange={setQuery}
          onSearch={(q) => void triggerSearch(q, category, 1)}
          onOpenTelemetry={() => setIsTelemetryOpen(true)}
          onToggleTheme={toggleTheme}
        />
      ) : (
        <>
          <SearchHeader
            query={query}
            category={category}
            timeFilter={timeFilter}
            telemetry={telemetry}
            isDarkMode={isDarkMode}
            suggestions={suggestions}
            mode={mode}
            onModeChange={setMode}
            onQueryChange={setQuery}
            onSearch={(q) => void triggerSearch(q, category, 1)}
            onSelectCategory={setCategory}
            onTimeFilterChange={setTimeFilter}
            onResetHome={resetToHome}
            onOpenTelemetry={() => setIsTelemetryOpen(true)}
            onToggleTheme={toggleTheme}
          />

          <main className="hx-results-layout">
            <section className="hx-results-stream">
              <div className="hx-stats-bar">{statsText}</div>

              <SearchResultsView
                usage={usage}
                results={results}
                isLoading={isLoading}
                category={category}
                query={query}
                searchError={searchError}
                baseUrl={baseUrl}
                engine={engineActive}
                coverage={coverage}
                stopReason={stopReason}
                answer={answer}
                llmAttempts={llmAttempts}
                trace={trace}
                onRetry={retry}
              />

              {showPagination && (
                <SearchPagination
                  currentPage={page}
                  totalMatches={metrics?.totalMatches ?? results.length}
                  pageSize={metrics?.pageSize ?? 6}
                  onPageChange={handlePageChange}
                />
              )}
            </section>
          </main>

          <SearchFooter telemetry={telemetry} onOpenTelemetry={() => setIsTelemetryOpen(true)} />
        </>
      )}

      <TelemetryModal
        isOpen={isTelemetryOpen}
        telemetry={telemetry}
        audit={audit}
        onClose={() => setIsTelemetryOpen(false)}
        onRefreshProbe={() => void refreshTelemetry()}
      />
    </div>
  );
};

export default App;
