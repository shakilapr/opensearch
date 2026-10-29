/**
 * @file SearchResultsView.tsx
 * @description Presentational results stream: loading, honest error state, empty
 * state, engine coverage strip, optional generated-answer panel, and the result
 * list. Holds no state and performs no I/O. Zero use of `any`.
 */

import React from 'react';
import { EngineCoverage, LlmAttemptView, SearchCategory, SearchResult, TokenUsageView } from '../domain/models';
import { describeSearchFailure, SearchUnavailableError } from '../services/searchErrors';
import { AnswerPanel } from './AnswerPanel';
import { EngineStatusStrip } from './EngineStatusStrip';
import { ImageResultGrid } from './ImageResultGrid';
import { SearchItem } from './SearchItem';

interface SearchResultsViewProps {
  readonly results: readonly SearchResult[];
  readonly isLoading: boolean;
  readonly category: SearchCategory;
  readonly query: string;
  readonly searchError: SearchUnavailableError | null;
  readonly baseUrl: string;
  readonly engine: boolean;
  readonly coverage: EngineCoverage | null;
  readonly stopReason: string | null;
  readonly answer: string | null;
  /** The models tried for the answer, in order: which failed and which answered. */
  readonly llmAttempts: readonly LlmAttemptView[];
  /** The engine's own trace, used for the plan summary in the status strip. */
  readonly trace: readonly string[];
  readonly usage?: TokenUsageView | null;
  readonly onRetry: () => void;
}

export const SearchResultsView: React.FC<SearchResultsViewProps> = ({
  results,
  isLoading,
  category,
  query,
  searchError,
  baseUrl,
  engine,
  coverage,
  stopReason,
  answer,
  llmAttempts,
  trace,
  usage,
  onRetry,
}) => {
  const answering = llmAttempts.find((attempt) => attempt.ok);
  const answeringModel = answering ? `${answering.provider}/${answering.model}` : null;
  if (isLoading) {
    return (
      <div className="empty-state-container">
        <p>Searching index...</p>
      </div>
    );
  }

  if (searchError !== null) {
    return (
      <div className="hx-error-state" data-testid="search-unavailable" role="alert">
        <h2 className="hx-error-title">Search unavailable</h2>
        <p className="hx-error-message">{searchError.message}</p>
        <p className="hx-error-detail">{describeSearchFailure(searchError)}</p>
        <p className="hx-error-hint">
          Configured base URL: <code className="font-mono">{baseUrl}</code>. Start the local API
          (<code className="font-mono">agent0 serve</code>) or set{' '}
          <code className="font-mono">VITE_API_BASE</code>, then retry.
        </p>
        <button type="button" onClick={onRetry} className="hx-error-retry">
          Retry
        </button>
      </div>
    );
  }

  const showEmptyState = results.length === 0;

  return (
    <div className="hx-results-view">
      <EngineStatusStrip
        engine={engine}
        coverage={coverage}
        stopReason={stopReason}
        llmAttempts={llmAttempts}
        trace={trace}
      />

      {answer !== null && answer.length > 0 && <AnswerPanel answer={answer} model={answeringModel} usage={usage} />}

      {showEmptyState ? (
        <div className="empty-state-container" data-testid="no-results">
          <p>
            No results for this query{query.trim().length > 0 ? `: “${query.trim()}”` : ''}.
          </p>
        </div>
      ) : category === 'images' ? (
        <ImageResultGrid results={results} />
      ) : (
        <div className="hx-results-list">
          {results.map((item) => (
            <SearchItem key={item.id} result={item} query={query} />
          ))}
        </div>
      )}
    </div>
  );
};
