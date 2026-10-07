/**
 * @file SearchPagination.tsx
 * @description Lightweight Bounded Pagination Control (< 60 LOC).
 * Ensures pagination is always accessible on results view.
 * Zero use of `any`.
 */

import React from 'react';

interface SearchPaginationProps {
  readonly currentPage: number;
  readonly totalMatches: number;
  readonly pageSize: number;
  readonly onPageChange: (p: number) => void;
}

export const SearchPagination: React.FC<SearchPaginationProps> = ({
  currentPage,
  totalMatches,
  pageSize,
  onPageChange,
}) => {
  const calculatedPages = Math.ceil(Math.max(totalMatches, pageSize * 3) / pageSize);
  const maxPages = Math.min(6, Math.max(2, calculatedPages));

  const pages: number[] = [];
  for (let p = 1; p <= maxPages; p += 1) {
    pages.push(p);
  }

  return (
    <nav className="hx-pagination" aria-label="Search Results Pagination">
      <button
        type="button"
        disabled={currentPage <= 1}
        onClick={() => onPageChange(currentPage - 1)}
        className="hx-page-btn"
      >
        Prev
      </button>

      <div className="hx-page-numbers">
        {pages.map((p) => (
          <button
            key={p}
            type="button"
            onClick={() => onPageChange(p)}
            className={`hx-page-num ${p === currentPage ? 'hx-page-active' : ''}`}
            aria-current={p === currentPage ? 'page' : undefined}
          >
            {p}
          </button>
        ))}
      </div>

      <button
        type="button"
        disabled={currentPage >= maxPages}
        onClick={() => onPageChange(currentPage + 1)}
        className="hx-page-btn"
      >
        Next
      </button>
    </nav>
  );
};
