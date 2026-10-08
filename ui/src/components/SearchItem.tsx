/**
 * @file SearchItem.tsx
 * @description Pure Search Result Item adhering to NASA JPL Rule 4 (< 60 LOC).
 * Clean typographic layout with zero card frames, zero emojis, and pure SVG iconography.
 * Zero use of `any`.
 */

import React from 'react';
import { highlightKeywords } from '../core/sanitizer';
import { SearchResult, SearchSignal } from '../domain/models';

interface SearchItemProps {
  readonly result: SearchResult;
  readonly query: string;
}

/** Bounded, locale-stable rendering of a signal's numeric value. */
function formatSignalValue(value: number): string {
  if (!Number.isFinite(value)) {
    return '0.000';
  }
  return value.toFixed(3);
}

const SignalRow: React.FC<{ readonly signal: SearchSignal }> = ({ signal }) => (
  <li className="hx-signal-item">
    <span className="hx-signal-name">{signal.name}</span>
    <span className="hx-signal-value">{formatSignalValue(signal.value)}</span>
    <span className="hx-signal-note">{signal.note}</span>
  </li>
);

export const SearchItem: React.FC<SearchItemProps> = ({ result, query }) => {
  const fragments = highlightKeywords(result.snippet, query);
  const signals = result.signals ?? [];

  return (
    <article className="hx-result-row">
      <div className="hx-result-source">
        <span className="hx-result-monogram">{result.sourceDomain.charAt(0).toUpperCase()}</span>
        <div className="hx-result-domain-col">
          <span className="hx-result-domain">{result.sourceDomain}</span>
          <cite className="hx-result-url">{result.displayUrl}</cite>
        </div>
        {result.liveOverlay === true && (
          <span className="hx-live-badge" role="status" aria-label="Result from the live overlay window">
            live
          </span>
        )}
      </div>

      <h2 className="hx-result-heading">
        <a href={result.url} target="_blank" rel="noopener noreferrer" className="hx-result-link">
          {result.title}
        </a>
      </h2>

      <p className="hx-result-snippet">
        {fragments.map((frag, idx) =>
          frag.isMatch ? (
            <mark key={idx} className="hx-snippet-match">{frag.text}</mark>
          ) : (
            <span key={idx}>{frag.text}</span>
          )
        )}
      </p>

      {result.sitelinks && result.sitelinks.length > 0 && (
        <div className="hx-sitelinks-list">
          {result.sitelinks.map((link) => (
            <div key={link.url} className="hx-sitelink-item">
              <a href={link.url} target="_blank" rel="noopener noreferrer" className="hx-sitelink-title">
                {link.title}
              </a>
              <p className="hx-sitelink-desc">{link.snippet}</p>
            </div>
          ))}
        </div>
      )}

      {signals.length > 0 && (
        <details className="hx-signals">
          <summary className="hx-signals-summary">Why this result?</summary>
          <ul className="hx-signals-list" aria-label="Ranking signals">
            {signals.map((signal) => (
              <SignalRow key={signal.name} signal={signal} />
            ))}
          </ul>
        </details>
      )}
    </article>
  );
};
