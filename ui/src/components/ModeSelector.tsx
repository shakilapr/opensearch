/**
 * @file ModeSelector.tsx
 * @description Small segmented control for the agentic engine mode (fast / auto /
 * deep). Purely presentational. Zero use of `any`.
 */

import React from 'react';
import { EngineMode } from '../domain/models';

interface ModeSelectorProps {
  readonly mode: EngineMode;
  readonly onModeChange: (mode: EngineMode) => void;
}

const MODES: readonly { readonly value: EngineMode; readonly label: string }[] = [
  { value: 'fast', label: 'Fast' },
  { value: 'auto', label: 'Auto' },
  { value: 'deep', label: 'Deep' },
];

export const ModeSelector: React.FC<ModeSelectorProps> = ({ mode, onModeChange }) => (
  <div className="hx-mode-selector" role="group" aria-label="Search engine mode">
    <span className="hx-mode-label">Mode</span>
    {MODES.map((entry) => (
      <button
        key={entry.value}
        type="button"
        onClick={() => onModeChange(entry.value)}
        className={`hx-mode-btn ${mode === entry.value ? 'hx-mode-active' : ''}`}
        aria-pressed={mode === entry.value}
      >
        {entry.label}
      </button>
    ))}
  </div>
);
