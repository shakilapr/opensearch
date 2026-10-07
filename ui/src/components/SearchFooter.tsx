/**
 * @file SearchFooter.tsx
 * @description Minimalist Footer (< 60 LOC).
 * Zero emojis, clean metadata links, and ISO quality attribution.
 * Zero use of `any`.
 */

import React from 'react';
import { NodeHealthTelemetry } from '../domain/models';

interface SearchFooterProps {
  readonly telemetry: NodeHealthTelemetry;
  readonly onOpenTelemetry: () => void;
}

export const SearchFooter: React.FC<SearchFooterProps> = ({ telemetry, onOpenTelemetry }) => {
  return (
    <footer className="hx-results-footer">
      <div className="hx-footer-inner">
        <div className="hx-footer-left">
          <span className="hx-node-status">Target: {telemetry.endpoint} ({telemetry.latencyMs}ms)</span>
          <span>•</span>
          <span>Hexxica Space Network © 2026</span>
        </div>
        <div className="hx-footer-right">
          <button type="button" onClick={onOpenTelemetry} className="hx-footer-btn">
            NASA JPL Rules of Ten
          </button>
          <span>•</span>
          <button type="button" onClick={onOpenTelemetry} className="hx-footer-btn">
            ISO/IEC 25010 Assured
          </button>
        </div>
      </div>
    </footer>
  );
};
