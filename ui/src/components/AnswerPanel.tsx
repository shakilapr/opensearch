/**
 * @file AnswerPanel.tsx
 * @description Renders the optional model-generated `answer` returned by the
 * agentic engine. The answer text comes from the server and is only displayed
 * here — the UI never generates it. Plain styling, explicit disclaimer, and
 * transparent display of the model and token usage.
 * Zero use of `any`.
 */

import React from 'react';
import { TokenUsageView } from '../domain/models';

interface AnswerPanelProps {
  readonly answer: string;
  readonly model?: string | null;
  readonly usage?: TokenUsageView | null;
}

export const AnswerPanel: React.FC<AnswerPanelProps> = ({ answer, model, usage }) => (
  <section className="hx-answer-panel" aria-label="Generated answer">
    <div className="hx-answer-header" style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', flexWrap: 'wrap', gap: '6px' }}>
      <span>Generated answer from indexed evidence</span>
      {model && (
        <span
          style={{
            fontFamily: 'monospace',
            textTransform: 'none',
            fontSize: '0.72rem',
            color: 'var(--hx-accent, #3b82f6)',
            backgroundColor: 'rgba(59, 130, 246, 0.1)',
            padding: '2px 8px',
            borderRadius: '4px',
            border: '1px solid rgba(59, 130, 246, 0.25)',
          }}
        >
          🤖 {model}
          {usage && (usage.input > 0 || usage.output > 0)
            ? ` • ${usage.input.toLocaleString()} in / ${usage.output.toLocaleString()} out tokens`
            : ''}
        </span>
      )}
    </div>
    <p className="hx-answer-body">{answer}</p>
    <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', flexWrap: 'wrap', gap: '8px', marginTop: '0.5rem' }}>
      <p className="hx-answer-disclaimer" style={{ margin: 0 }}>
        This answer is model-generated and may be incomplete. Always verify against the sources below.
      </p>
      {model && (
        <div style={{ fontSize: '0.72rem', color: 'var(--hx-text-muted)' }}>
          Model: <code>{model}</code>
          {usage && (usage.input > 0 || usage.output > 0)
            ? ` • Tokens: ${usage.input.toLocaleString()} in, ${usage.output.toLocaleString()} out`
            : ''}
        </div>
      )}
    </div>
  </section>
);
