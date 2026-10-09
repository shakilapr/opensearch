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
              <div className="hx-stats-bar">
                {metrics !== null
                  ? `${metrics.totalMatches} results (${metrics.executionTimeMs}ms) • Node: ${baseUrl}`
                  : `Node: ${baseUrl}`}
              </div>

              <SearchResultsView
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
