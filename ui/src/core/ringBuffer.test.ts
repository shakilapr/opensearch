/**
 * @file ringBuffer.test.ts
 * @description NASA JPL Rule 3 Bounded Ring Buffer Test Suite.
 * Zero use of `any`.
 */

import { describe, expect, it } from 'vitest';
import { FixedRingBuffer } from './ringBuffer';

describe('FixedRingBuffer NASA Rule 3 Compliance', () => {
  it('respects fixed capacity bounds and overwrites oldest on overflow', () => {
    const buffer = new FixedRingBuffer<string>(3);
    expect(buffer.size()).toBe(0);

    buffer.push('alpha');
    buffer.push('beta');
    buffer.push('gamma');
    expect(buffer.size()).toBe(3);

    const snapshot1 = buffer.toArray();
    expect(snapshot1).toEqual(['gamma', 'beta', 'alpha']);

    // Push 4th element (should drop oldest 'alpha')
    buffer.push('delta');
    expect(buffer.size()).toBe(3);
    const snapshot2 = buffer.toArray();
    expect(snapshot2).toEqual(['delta', 'gamma', 'beta']);
  });

  it('supports clear operation and resets pointers', () => {
    const buffer = new FixedRingBuffer<number>(5);
    buffer.push(10);
    buffer.push(20);
    expect(buffer.size()).toBe(2);

    buffer.clear();
    expect(buffer.size()).toBe(0);
    expect(buffer.toArray()).toEqual([]);
  });
});
