/**
 * @file HexxicaBrand.tsx
 * @description Modern Minimalist Hexxica Brand Identity (< 60 LOC).
 * Features custom vector geometric glyph and crisp typography without Google styling.
 * Zero use of `any`.
 */

import React from 'react';
import { OrbitIcon } from './Icons';

interface HexxicaBrandProps {
  readonly size?: 'large' | 'compact';
  readonly onClick?: () => void;
}

export const HexxicaBrand: React.FC<HexxicaBrandProps> = ({ size = 'large', onClick }) => {
  const isLarge = size === 'large';

  return (
    <div
      onClick={onClick}
      className={`hx-brand-wrapper ${isLarge ? 'hx-brand-large' : 'hx-brand-compact'} ${onClick ? 'hx-clickable' : ''}`}
      role={onClick ? 'button' : undefined}
      tabIndex={onClick ? 0 : undefined}
      aria-label="Hexxica Search Home"
    >
      <div className="hx-brand-icon-box">
        <OrbitIcon size={isLarge ? 32 : 20} className="hx-brand-glyph" />
      </div>
      <div className="hx-brand-text-box">
        <span className="hx-brand-title">HEXXICA</span>
        {isLarge && <span className="hx-brand-tagline">DEEP INDEX // TELEMETRY</span>}
      </div>
    </div>
  );
};
