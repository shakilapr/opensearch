/**
 * @file ImageResultGrid.tsx
 * @description Orbital Imagery & Sensor Grid (< 60 LOC).
 * Zero use of `any`.
 */

import React from 'react';
import { SearchResult } from '../domain/models';

interface ImageResultGridProps {
  readonly results: readonly SearchResult[];
}

export const ImageResultGrid: React.FC<ImageResultGridProps> = ({ results }) => {
  const imageResults = results.filter((r) => r.imageUrl !== undefined);

  if (imageResults.length === 0) {
    return (
      <div className="empty-state-container">
        <p>No orbital imagery assets located for this query parameter.</p>
      </div>
    );
  }

  return (
    <div className="image-grid-container">
      {imageResults.map((item) => (
        <a
          key={item.id}
          href={item.url}
          target="_blank"
          rel="noopener noreferrer"
          className="image-card"
        >
          <div className="image-wrapper">
            <img src={item.imageUrl} alt={item.title} loading="lazy" className="image-asset" />
            <span className="image-overlay-badge">{item.sourceDomain}</span>
          </div>
          <div className="image-card-caption">
            <h3 className="image-card-title">{item.title}</h3>
            {item.aerospaceTelemetry && (
              <p className="image-card-sub">{item.aerospaceTelemetry.mission}</p>
            )}
          </div>
        </a>
      ))}
    </div>
  );
};
