/**
 * @file EngineStatusStrip.tsx
 * @description A plain coverage / stop-reason strip summarising the agentic
 * engine's view of a response. Renders nothing when the engine reported no
 * metadata (older server). Zero use of `any`.
 */

import React from 'react';
import { EngineCoverage, LlmAttemptView } from '../domain/models';

interface EngineStatusStripProps {
  readonly engine: boolean;
  readonly coverage: EngineCoverage | null;
  readonly stopReason: string | null;
  /** The models tried for the answer, in order: which failed and which answered. */
  readonly llmAttempts: readonly LlmAttemptView[];
  /** The engine's own trace lines; the `plan:` line is surfaced as the search plan summary. */
  readonly trace: readonly string[];
}

/** Format a 0..1 coverage fraction as a 2-decimal string. */
function formatCoverage(value: number | null): string | null {
  if (value === null || !Number.isFinite(value)) {
    return null;
  }
  return value.toFixed(2);
}

/**
 * One line per model the engine tried, in order, marking the one that answered. A failed attempt
 * carries the provider's own error, so "no answer" is distinguishable from "the model said no".
 */
function describeAttempts(attempts: readonly LlmAttemptView[]): string | null {
  if (attempts.length === 0) {
    return null;
  }
  const parts = attempts.map((attempt) => {
    const name = `${attempt.provider}/${attempt.model}`;
    if (attempt.ok) {
      return `${name} (answered)`;
    }
    return attempt.error !== null ? `${name} (failed: ${attempt.error})` : `${name} (failed)`;
  });
  return `models tried: ${parts.join(' → ')}`;
}

export const EngineStatusStrip: React.FC<EngineStatusStripProps> = ({
  engine,
  coverage,
  stopReason,
  llmAttempts,
  trace,
}) => {
  const parts: string[] = [];

  if (engine) {
    parts.push('engine: on');
  }
  // The engine's own plan line: mode, lanes, per-lane top-k, query variants and rerank width.
  const planLine = trace.find((line) => line.startsWith('plan:'));
  if (planLine !== undefined) {
    parts.push(planLine);
  }
  const attempts = describeAttempts(llmAttempts);
  if (attempts !== null) {
    parts.push(attempts);
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
