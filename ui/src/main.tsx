/**
 * @file main.tsx
 * @description Application Bootstrap Entry Point.
 * ISO/IEC 25010 & NASA JPL Standards Compliant.
 * Zero use of `any`.
 */

import React from 'react';
import ReactDOM from 'react-dom/client';
import App from './App';
import './styles/index.css';
import { assertNonNull } from './core/assertions';

const rootElement = document.getElementById('root');
assertNonNull(rootElement, 'rootElement');

ReactDOM.createRoot(rootElement).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>
);
