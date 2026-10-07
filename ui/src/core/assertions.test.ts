/**
 * @file assertions.test.ts
 * @description NASA JPL Rule 5 & ISO/IEC 25010 Test Suite.
 * Zero use of `any`.
 */

import { describe, expect, it } from 'vitest';
import {
  AssertionViolationError,
  assertBoundedString,
  assertCondition,
  assertInRange,
  assertNonNull,
} from './assertions';

describe('NASA JPL Assertions Suite', () => {
  it('assertCondition throws on false condition', () => {
    expect(() => assertCondition(false, 'Failure expected')).toThrow(AssertionViolationError);
    expect(() => assertCondition(true, 'Nominal')).not.toThrow();
  });

  it('assertNonNull protects against null and undefined dereferences', () => {
    expect(() => assertNonNull(null, 'nullParam')).toThrow(AssertionViolationError);
    expect(() => assertNonNull(undefined, 'undefinedParam')).toThrow(AssertionViolationError);
    expect(() => assertNonNull('nominalValue', 'validParam')).not.toThrow();
  });

  it('assertInRange validates bounded numeric ranges', () => {
    expect(() => assertInRange(5, 0, 10, 'inRange')).not.toThrow();
    expect(() => assertInRange(-1, 0, 10, 'outOfRangeLow')).toThrow(AssertionViolationError);
    expect(() => assertInRange(15, 0, 10, 'outOfRangeHigh')).toThrow(AssertionViolationError);
    expect(() => assertInRange(NaN, 0, 10, 'nanValue')).toThrow(AssertionViolationError);
  });

  it('assertBoundedString guards against buffer overrun', () => {
    expect(() => assertBoundedString('test', 10, 'bounded')).not.toThrow();
    expect(() => assertBoundedString('overflow-string-value', 5, 'tooLong')).toThrow(
      AssertionViolationError
    );
  });
});
