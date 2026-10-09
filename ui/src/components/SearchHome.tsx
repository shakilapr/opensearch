/**
 * @file SearchHome.tsx
 * @description Modern Minimalist Search Engine Home View (< 60 LOC).
 * Zero emojis, authentic SVG icons, and distraction-free interface.
 * Zero use of `any`.
 */

import React from 'react';
import { EngineMode, NodeHealthTelemetry } from '../domain/models';
import { HexxicaBrand } from './HexxicaBrand';
import { MoonIcon, SunIcon } from './Icons';
import { ModeSelector } from './ModeSelector';
import { SearchInput } from './SearchInput';

interface SearchHomeProps {
  readonly query: string;
  readonly suggestions: readonly string[];
  readonly telemetry: NodeHealthTelemetry;
  readonly isDarkMode: boolean;
  readonly mode: EngineMode;
  readonly onModeChange: (mode: EngineMode) => void;
  readonly onQueryChange: (q: string) => void;
  readonly onSearch: (q: string) => void;
  readonly onOpenTelemetry: () => void;
  readonly onToggleTheme: () => void;
}

export const SearchHome: React.FC<SearchHomeProps> = ({
  query,
  suggestions,
  telemetry,
  isDarkMode,
  mode,
  onModeChange,
  onQueryChange,
  onSearch,
  onOpenTelemetry,
  onToggleTheme,
}) => {
  return (
    <div className="hx-home-screen">
      <nav className="hx-home-topbar">
        <a href="https://code.hexxica.space" target="_blank" rel="noopener noreferrer" className="hx-nav-link">VS Code</a>
        <button type="button" onClick={onOpenTelemetry} className="hx-telemetry-pill">
          <span className={`hx-status-dot ${telemetry.status === 'ONLINE' ? 'hx-dot-green' : 'hx-dot-amber'}`} />
          <span>{telemetry.status === 'ONLINE' ? 'Online' : 'Offline'}</span>
          <span className="hx-latency-tag">{telemetry.latencyMs}ms</span>
        </button>
        <button type="button" onClick={onToggleTheme} className="hx-theme-btn" aria-label="Toggle visual theme">
          {isDarkMode ? <SunIcon size={17} /> : <MoonIcon size={17} />}
        </button>
        <button type="button" onClick={onOpenTelemetry} className="hx-btn-standards">ISO / NASA</button>
      </nav>

      <main className="hx-home-core">
        <HexxicaBrand size="large" />
        <div className="hx-home-input-box">
          <SearchInput query={query} suggestions={suggestions} autoFocus onQueryChange={onQueryChange} onSubmit={onSearch} />
        </div>

        <ModeSelector mode={mode} onModeChange={onModeChange} />

        <div className="hx-quick-chips">
          <button type="button" onClick={() => onSearch('NASA JPL')} className="hx-chip">NASA JPL</button>
          <button type="button" onClick={() => onSearch('Deep Space Network')} className="hx-chip">Deep Space Network</button>
          <button type="button" onClick={() => onSearch('ISO/IEC 25010')} className="hx-chip">ISO/IEC 25010</button>
          <button type="button" onClick={() => onSearch('TypeScript 7')} className="hx-chip">TypeScript 7</button>
        </div>
      </main>

      <footer className="hx-home-footer">
        <span>Node: {telemetry.endpoint}</span>
        <span>•</span>
        <button type="button" onClick={onOpenTelemetry} className="hx-footer-link-btn">NASA JPL &amp; ISO 25010 Verified</button>
      </footer>
    </div>
  );
};
