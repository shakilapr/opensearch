/**
 * @file App.tsx
 * @description Ultra-Lightweight Minimalist Search Engine Shell.
 * Adheres to NASA JPL Rules of Ten and ISO/IEC 25010 Quality Model.
 * Zero emojis, SVG vector iconography, and zero use of `any`.
 */

import React, { useState } from 'react';
import { ImageResultGrid } from './components/ImageResultGrid';
import { SearchFooter } from './components/SearchFooter';
import { SearchHeader } from './components/SearchHeader';
import { SearchHome } from './components/SearchHome';
import { SearchItem } from './components/SearchItem';
import { SearchPagination } from './components/SearchPagination';
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
    page,
    results,
    metrics,
    suggestions,
    audit,
    isLoading,
    hasSubmitted,
    setQuery,
    setCategory,
    setTimeFilter,
    triggerSearch,
    handlePageChange,
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

  return (
    <div className="hx-app" data-theme={isDarkMode ? 'dark' : 'light'}>
      {isHome ? (
        <SearchHome
          query={query}
          suggestions={suggestions}
          telemetry={telemetry}
          isDarkMode={isDarkMode}
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
                {metrics?.totalMatches ?? results.length} results ({metrics?.executionTimeMs ?? 24}ms) • Node: {telemetry.endpoint}
              </div>

              {isLoading ? (
                <div className="empty-state-container">
                  <p>Searching index...</p>
                </div>
              ) : category === 'images' ? (
                <ImageResultGrid results={results} />
              ) : (
                <>
                  <div className="hx-results-list">
                    {results.map((item) => (
                      <SearchItem key={item.id} result={item} query={query} />
                    ))}
                  </div>

                  <SearchPagination
                    currentPage={page}
                    totalMatches={metrics?.totalMatches ?? results.length}
                    pageSize={metrics?.pageSize ?? 6}
                    onPageChange={handlePageChange}
                  />
                </>
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
