/**
 * @file SearchHeader.tsx
 * @description Minimalist Search Engine Results Header (< 60 LOC).
 * Zero emojis, SVG vector icons, and clean category navigation.
 * Zero use of `any`.
 */

import React, { useState } from 'react';
import { NodeHealthTelemetry, SearchCategory, TimeFilter } from '../domain/models';
import { HexxicaBrand } from './HexxicaBrand';
import { MoonIcon, SunIcon } from './Icons';
import { SearchInput } from './SearchInput';

interface SearchHeaderProps {
  readonly query: string;
  readonly category: SearchCategory;
  readonly timeFilter: TimeFilter;
  readonly telemetry: NodeHealthTelemetry;
  readonly isDarkMode: boolean;
  readonly suggestions: readonly string[];
  readonly onQueryChange: (q: string) => void;
  readonly onSearch: (q: string) => void;
  readonly onSelectCategory: (c: SearchCategory) => void;
  readonly onTimeFilterChange: (t: TimeFilter) => void;
  readonly onResetHome: () => void;
  readonly onOpenTelemetry: () => void;
  readonly onToggleTheme: () => void;
}

export const SearchHeader: React.FC<SearchHeaderProps> = ({
  query,
  category,
  timeFilter,
  telemetry,
  isDarkMode,
  suggestions,
  onQueryChange,
  onSearch,
  onSelectCategory,
  onTimeFilterChange,
  onResetHome,
  onOpenTelemetry,
  onToggleTheme,
}) => {
  const [showTools, setShowTools] = useState(false);

  return (
    <header className="hx-top-header">
      <div className="hx-header-main">
        <HexxicaBrand size="compact" onClick={onResetHome} />
        <div className="hx-header-input-wrap">
          <SearchInput query={query} suggestions={suggestions} onQueryChange={onQueryChange} onSubmit={onSearch} />
        </div>
        <div className="hx-header-actions">
          <button type="button" onClick={onOpenTelemetry} className="hx-telemetry-pill" title={telemetry.diagnosticsMessage}>
            <span className={`hx-status-dot ${telemetry.status === 'ONLINE' ? 'hx-dot-green' : 'hx-dot-amber'}`} />
            <span>{telemetry.status === 'ONLINE' ? 'Online' : 'Simulation'}</span>
            <span className="hx-latency-tag">{telemetry.latencyMs}ms</span>
          </button>
          <button type="button" onClick={onToggleTheme} className="hx-theme-btn" aria-label="Toggle visual theme">
            {isDarkMode ? <SunIcon size={17} /> : <MoonIcon size={17} />}
          </button>
          <button type="button" onClick={onOpenTelemetry} className="hx-btn-standards">ISO / NASA</button>
        </div>
      </div>

      <nav className="hx-header-nav">
        <div className="hx-tabs-list">
          <button type="button" onClick={() => onSelectCategory('all')} className={`hx-tab ${category === 'all' ? 'hx-tab-active' : ''}`}>All</button>
          <button type="button" onClick={() => onSelectCategory('space')} className={`hx-tab ${category === 'space' ? 'hx-tab-active' : ''}`}>Space</button>
          <button type="button" onClick={() => onSelectCategory('tech')} className={`hx-tab ${category === 'tech' ? 'hx-tab-active' : ''}`}>Tech</button>
          <button type="button" onClick={() => onSelectCategory('news')} className={`hx-tab ${category === 'news' ? 'hx-tab-active' : ''}`}>News</button>
          <button type="button" onClick={() => onSelectCategory('images')} className={`hx-tab ${category === 'images' ? 'hx-tab-active' : ''}`}>Images</button>
          <button type="button" onClick={() => setShowTools((v) => !v)} className={`hx-tab hx-tools-toggle ${showTools ? 'hx-tab-active' : ''}`}>Tools</button>
        </div>

        {showTools && (
          <div className="hx-tools-row">
            <select value={timeFilter} onChange={(e) => onTimeFilterChange(e.target.value as TimeFilter)} className="hx-filter-select">
              <option value="all">Any time</option>
              <option value="24h">Past 24 hours</option>
              <option value="7d">Past week</option>
              <option value="30d">Past month</option>
            </select>
          </div>
        )}
      </nav>
    </header>
  );
};
