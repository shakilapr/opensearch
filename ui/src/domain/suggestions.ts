/**
 * @file suggestions.ts
 * @description Local autocomplete suggestion strings for the search box. These
 * are completion hints only — never search results. The UI surfaces results
 * exclusively from the real API; nothing here is ever rendered as a result.
 * Zero use of `any`.
 */

export const DEFAULT_SUGGESTIONS: readonly string[] = Object.freeze([
  'NASA JPL Institutional Coding Standards',
  'Deep Space Network antenna array',
  'James Webb Space Telescope infrared imagery',
  'ISO/IEC 25010 software quality model',
  'Voyager 1 interstellar plasma telemetry',
  'Mars Perseverance Jezero crater samples',
  'TypeScript 7 strict architectural patterns',
]);
