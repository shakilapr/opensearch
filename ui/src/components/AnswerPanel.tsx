/**
 * @file AnswerPanel.tsx
 * @description Renders the optional model-generated `answer` returned by the
 * agentic engine. The answer text comes from the server and is only displayed
 * here — the UI never generates it. Plain styling, explicit disclaimer.
 * Zero use of `any`.
 */

import React from 'react';

interface AnswerPanelProps {
  readonly answer: string;
}

export const AnswerPanel: React.FC<AnswerPanelProps> = ({ answer }) => (
  <section className="hx-answer-panel" aria-label="Generated answer">
    <div className="hx-answer-header">Generated answer from indexed evidence</div>
    <p className="hx-answer-body">{answer}</p>
    <p className="hx-answer-disclaimer">
      This answer is model-generated and may be incomplete. Always verify against the sources below.
    </p>
  </section>
);
