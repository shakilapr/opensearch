/**
 * @file sanitizer.test.ts
 * @description ISO/IEC 25010 Security & Input Sanitization Test Suite.
 * Zero use of `any`.
 */

import { describe, expect, it } from 'vitest';
import { highlightKeywords, sanitizeSearchQuery } from './sanitizer';

describe('Sanitizer & Keyword Matcher Suite', () => {
  it('strips hazardous injection script characters and bounds query length', () => {
    const malicious = '<script>alert("xss")</script>; DROP TABLE users;';
    const sanitized = sanitizeSearchQuery(malicious);

    expect(sanitized).not.toContain('<');
    expect(sanitized).not.toContain('>');
    expect(sanitized).not.toContain(';');
    expect(sanitized).toBe('script alert xss /script DROP TABLE users');
  });

  it('bounds overly long query strings', () => {
    const longString = 'a'.repeat(300);
    const sanitized = sanitizeSearchQuery(longString);
    expect(sanitized.length).toBeLessThanOrEqual(120);
  });

  it('highlights matched keywords deterministically', () => {
    const text = 'NASA Perseverance Rover telemetry stream';
    const fragments = highlightKeywords(text, 'Perseverance telemetry');

    expect(fragments.some((f) => f.text.toLowerCase() === 'perseverance' && f.isMatch)).toBe(true);
    expect(fragments.some((f) => f.text.toLowerCase() === 'telemetry' && f.isMatch)).toBe(true);
    expect(fragments.some((f) => f.text === 'NASA ' && !f.isMatch)).toBe(true);
  });
});
