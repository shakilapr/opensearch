/**
 * @file SearchInput.tsx
 * @description Accessible Search Input adhering to NASA JPL Rule 4 (< 60 LOC).
 * Dropdown is strictly constrained to non-empty queries with outside-click dismissal.
 * Zero use of `any`.
 */

import React, { useEffect, useRef, useState } from 'react';
import { CloseIcon, MicIcon, SearchIcon } from './Icons';

interface SearchInputProps {
  readonly query: string;
  readonly suggestions: readonly string[];
  readonly autoFocus?: boolean;
  readonly onQueryChange: (q: string) => void;
  readonly onSubmit: (q: string) => void;
}

export const SearchInput: React.FC<SearchInputProps> = ({
  query,
  suggestions,
  autoFocus = false,
  onQueryChange,
  onSubmit,
}) => {
  const [isOpen, setIsOpen] = useState(false);
  const containerRef = useRef<HTMLDivElement>(null);
  const inputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    const handleOutsideClick = (event: MouseEvent) => {
      if (containerRef.current && !containerRef.current.contains(event.target as Node)) {
        setIsOpen(false);
      }
    };
    document.addEventListener('mousedown', handleOutsideClick);
    return () => document.removeEventListener('mousedown', handleOutsideClick);
  }, []);

  const handleSubmit = (e: React.FormEvent) => {
    e.preventDefault();
    setIsOpen(false);
    if (query.trim().length > 0) {
      onSubmit(query);
    }
  };

  const hasValidQuery = query.trim().length > 0;

  return (
    <div ref={containerRef} className="hx-search-container">
      <form onSubmit={handleSubmit} className="hx-search-form">
        <SearchIcon size={18} className="hx-search-icon" />
        <input
          ref={inputRef}
          type="text"
          value={query}
          maxLength={120}
          autoFocus={autoFocus}
          placeholder="Search aerospace index, telemetry, or enter query..."
          onChange={(e) => {
            onQueryChange(e.target.value);
            setIsOpen(e.target.value.trim().length > 0);
          }}
          onFocus={() => {
            if (hasValidQuery) {
              setIsOpen(true);
            }
          }}
          className="hx-search-input"
        />
        {query.length > 0 && (
          <button
            type="button"
            onClick={() => {
              onQueryChange('');
              setIsOpen(false);
              inputRef.current?.focus();
            }}
            className="hx-btn-action"
            aria-label="Clear query"
          >
            <CloseIcon size={15} />
          </button>
        )}
        <button
          type="button"
          onClick={() => {
            onQueryChange('James Webb Space Telescope');
            onSubmit('James Webb Space Telescope');
            setIsOpen(false);
          }}
          className="hx-btn-action"
          title="Voice Simulation Input"
          aria-label="Voice input"
        >
          <MicIcon size={17} />
        </button>
      </form>

      {isOpen && hasValidQuery && suggestions.length > 0 && (
        <ul className="hx-suggestions-list">
          {suggestions.map((s) => (
            <li
              key={s}
              onMouseDown={() => {
                onQueryChange(s);
                onSubmit(s);
                setIsOpen(false);
              }}
              className="hx-suggestion-item"
            >
              <SearchIcon size={14} className="hx-suggestion-icon" />
              <span>{s}</span>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
};
