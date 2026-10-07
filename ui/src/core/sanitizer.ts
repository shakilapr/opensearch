/**
 * @file sanitizer.ts
 * @description Input sanitization and bounded string processing.
 * ISO/IEC 25010 Security & NASA JPL Rule 2 Bounded Processing.
 * Zero use of `any`.
 */

import { assertBoundedString, assertNonNull } from './assertions';

export const MAX_SAFE_QUERY_LENGTH = 120;
export const MIN_SEARCH_QUERY_LENGTH = 1;

/**
 * Sanitizes and strips potentially hazardous control characters or script injections.
 */
export function sanitizeSearchQuery(rawInput: string): string {
  assertNonNull(rawInput, 'rawInput');
  
  // Truncate safely to upper bound (NASA Rule 2)
  const boundedInput = rawInput.slice(0, MAX_SAFE_QUERY_LENGTH);
  
  // Sanitize non-printable control characters and dangerous HTML markup
  const clean = boundedInput
    .replace(/[<>'"`;(){}[\]\\]/g, ' ')
    .replace(/\s+/g, ' ')
    .trim();

  assertBoundedString(clean, MAX_SAFE_QUERY_LENGTH, 'cleanQuery');
  return clean;
}

/**
 * Highlights matches of query keywords in source text safely.
 */
export function highlightKeywords(
  text: string,
  query: string
): readonly { text: string; isMatch: boolean }[] {
  assertNonNull(text, 'text');
  assertNonNull(query, 'query');

  const trimmedQuery = query.trim();
  if (trimmedQuery.length === 0) {
    return [{ text, isMatch: false }];
  }

  const terms = trimmedQuery
    .toLowerCase()
    .split(/\s+/)
    .filter((term) => term.length > 0)
    .slice(0, 5); // Bounded term matching (NASA Rule 2)

  if (terms.length === 0) {
    return [{ text, isMatch: false }];
  }

  const pattern = new RegExp(`(${terms.map((t) => t.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')).join('|')})`, 'gi');
  const tokens = text.split(pattern);

  const fragments: { text: string; isMatch: boolean }[] = [];
  const maxFragments = 100; // Fixed loop bound (NASA Rule 2)

  for (let i = 0; i < Math.min(tokens.length, maxFragments); i += 1) {
    const token = tokens[i];
    if (token !== undefined && token.length > 0) {
      const isMatch = terms.some((term) => term.toLowerCase() === token.toLowerCase());
      fragments.push({ text: token, isMatch });
    }
  }

  return Object.freeze(fragments);
}
