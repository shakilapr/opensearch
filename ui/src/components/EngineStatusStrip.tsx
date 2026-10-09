/**
 * @file EngineStatusStrip.tsx
 * @description A plain coverage / stop-reason strip summarising the agentic
 * engine's view of a response. Renders nothing when the engine reported no
 * metadata (older server). Zero use of `any`.
 */

import React from 'react';
import { EngineCoverage } from '../domain/models';

interface EngineStatusStripProps {
  readonly engine: boolean;
  readonly coverage: EngineCoverage | null;
  readonly stopReason: string | null;
}

/** Format a 0..1 coverage fraction as a 2-decimal string. */
function formatCoverage(value: number | null): string | null {
  if (value === null || !Number.isFinite(value)) {
    return null;
  }
  return value.toFixed(2);
}

export const EngineStatusStrip: React.FC<EngineStatusStripProps> = ({
  engine,
  coverage,
  stopReason,
}) => {
  const parts: string[] = [];

  if (engine) {
    parts.push('engine: on');
  }
  if (coverage !== null) {
    const fraction = formatCoverage(coverage.coverage);
    if (fraction !== null) {
      parts.push(`coverage ${fraction}`);
    }
    if (coverage.mandatoryOk) {
      parts.push('mandatory predicates satisfied');
    }
    if (coverage.supported.length > 0) {
      parts.push(`supported: ${coverage.supported.join(', ')}`);
    }
    if (coverage.unsupported.length > 0) {
      parts.push(`unsupported: ${coverage.unsupported.join(', ')}`);
    }
  }
  if (stopReason !== null && stopReason.length > 0) {
    parts.push(`stopped: ${stopReason}`);
  }

  if (parts.length === 0) {
    return null;
  }

  return (
    <div className="hx-engine-strip" role="status" aria-label="Engine coverage and stop reason">
      {parts.join(' · ')}
    </div>
  );
};
