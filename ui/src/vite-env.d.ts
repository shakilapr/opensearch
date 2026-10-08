/// <reference types="vite/client" />

interface ImportMetaEnv {
  /** Base URL of the local search-api (default: http://127.0.0.1:8080). */
  readonly VITE_API_BASE?: string;
}

interface ImportMeta {
  readonly env: ImportMetaEnv;
}

declare module '*.css' {
  const content: Record<string, string>;
  export default content;
}
