/**
 * @file ringBuffer.ts
 * @description NASA JPL Rule 3: Fixed-capacity memory buffer.
 * Pre-allocates and bounds collections to prevent unbounded memory growth.
 * Zero use of `any`.
 */

import { assertCondition, assertInRange, assertNonNull } from './assertions';

export class FixedRingBuffer<T> {
  private readonly buffer: readonly T[];
  private readonly capacity: number;
  private head: number;
  private count: number;

  public constructor(capacity: number) {
    assertInRange(capacity, 1, 1000, 'capacity');
    this.capacity = capacity;
    this.buffer = new Array<T>(capacity);
    this.head = 0;
    this.count = 0;
  }

  /**
   * Pushes a new element into the ring buffer, overwriting oldest entry if full.
   */
  public push(item: T): void {
    assertNonNull(item, 'item');
    const targetArray = this.buffer as T[];
    targetArray[this.head] = item;
    this.head = (this.head + 1) % this.capacity;
    if (this.count < this.capacity) {
      this.count += 1;
    }
    assertCondition(this.count <= this.capacity, 'Ring buffer count invariant violated');
  }

  /**
   * Returns snapshot of elements from newest to oldest up to max limit.
   */
  public toArray(limit?: number): readonly T[] {
    const maxEntries = limit !== undefined ? Math.min(limit, this.count) : this.count;
    assertInRange(maxEntries, 0, this.capacity, 'maxEntries');

    const result: T[] = [];
    const sourceArray = this.buffer as readonly T[];

    // Fixed bounded loop (NASA Rule 2)
    for (let i = 0; i < maxEntries; i += 1) {
      const index = (this.head - 1 - i + this.capacity) % this.capacity;
      const element = sourceArray[index];
      if (element !== undefined) {
        result.push(element);
      }
    }

    return Object.freeze(result);
  }

  public size(): number {
    return this.count;
  }

  public clear(): void {
    const targetArray = this.buffer as T[];
    for (let i = 0; i < this.capacity; i += 1) {
      delete targetArray[i];
    }
    this.head = 0;
    this.count = 0;
  }
}
