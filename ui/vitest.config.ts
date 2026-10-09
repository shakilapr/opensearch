import { defineConfig } from 'vitest/config';

// Minimal Vitest setup: a jsdom environment so tests can exercise DOM-facing
// code, plus node globals for `fetch`/`Response` used by the API-client tests.
export default defineConfig({
  test: {
    environment: 'jsdom',
    globals: false,
    include: ['src/**/*.test.ts', 'src/**/*.test.tsx'],
  },
});
