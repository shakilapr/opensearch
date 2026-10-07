/**
 * @file assertions.ts
 * @description NASA JPL Institutional Coding Standard - Rule 5 & Rule 7:
 * Enforces rigorous runtime assertions, pre-condition / post-condition checks,
 * and parameter validity with zero usage of the `any` keyword.
 * @standard ISO/IEC 25010 Reliability & NASA JPL Rules of Ten
 */

export class AssertionViolationError extends Error {
  public readonly code: string;
  public readonly timestamp: number;

  public constructor(message: string, code = 'ERR_NASA_ASSERT_FAIL') {
    super(`[NASA-JPL-ASSERTION-FAILURE] ${message} (Code: ${code})`);
    this.name = 'AssertionViolationError';
    this.code = code;
    this.timestamp = Date.now();
  }
}

/**
 * Asserts a condition is truthy, throwing a structured AssertionViolationError otherwise.
 * NASA Rule 5: Strong assertion density.
 */
export function assertCondition(
  condition: boolean,
  errorMessage: string,
  code = 'ERR_CONDITION_FALSE'
): asserts condition {
  if (!condition) {
    throw new AssertionViolationError(errorMessage, code);
  }
}

/**
 * Asserts a reference is neither null nor undefined.
 * NASA Rule 9: Restrict pointer / reference dereferences.
 */
export function assertNonNull<T>(
  value: T | null | undefined,
  identifierName: string
): asserts value is NonNullable<T> {
  if (value === null || value === undefined) {
    throw new AssertionViolationError(
      `Reference '${identifierName}' cannot be null or undefined.`,
      'ERR_NULL_REFERENCE'
    );
  }
}

/**
 * Asserts a numeric value falls within a statically bounded inclusive range.
 * NASA Rule 2: Upper and lower bounds verification.
 */
export function assertInRange(
  val: number,
  min: number,
  max: number,
  parameterName: string
): void {
  assertCondition(
    !Number.isNaN(val) && Number.isFinite(val),
    `Value for '${parameterName}' must be a finite number.`,
    'ERR_NUMBER_NOT_FINITE'
  );
  assertCondition(
    val >= min && val <= max,
    `Value '${val}' for '${parameterName}' exceeds bounded range [${min}, ${max}].`,
    'ERR_RANGE_EXCEEDED'
  );
}

/**
 * Validates a string's length within fixed upper bounds.
 * NASA Rule 2: Bounded length protection against buffer overruns.
 */
export function assertBoundedString(
  value: string,
  maxLength: number,
  paramName: string
): void {
  assertNonNull(value, paramName);
  assertCondition(
    typeof value === 'string',
    `Parameter '${paramName}' must be of type string.`,
    'ERR_TYPE_MISMATCH'
  );
  assertCondition(
    value.length <= maxLength,
    `String '${paramName}' length (${value.length}) exceeds fixed upper bound (${maxLength}).`,
    'ERR_STRING_OVERRUN'
  );
}
