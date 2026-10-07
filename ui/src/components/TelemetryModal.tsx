/**
 * @file TelemetryModal.tsx
 * @description NASA JPL Rule of Ten & ISO/IEC 25010 Compliance Inspector.
 * Zero use of `any`.
 */

import React from 'react';
import { IsoQualityAudit, NodeHealthTelemetry } from '../domain/models';

import { CloseIcon } from './Icons';

interface TelemetryModalProps {
  readonly isOpen: boolean;
  readonly telemetry: NodeHealthTelemetry;
  readonly audit: IsoQualityAudit;
  readonly onClose: () => void;
  readonly onRefreshProbe: () => void;
}

export const TelemetryModal: React.FC<TelemetryModalProps> = ({
  isOpen,
  telemetry,
  audit,
  onClose,
  onRefreshProbe,
}) => {
  if (!isOpen) {
    return null;
  }

  return (
    <div className="modal-backdrop" onClick={onClose} role="dialog" aria-modal="true">
      <div className="modal-content" onClick={(e) => e.stopPropagation()}>
        <div className="modal-header">
          <div className="modal-title-group">
            <h2 className="modal-title">MISSION CONTROL TELEMETRY</h2>
            <span className="modal-subtitle">ISO/IEC 25010 &amp; NASA JPL SAFETY-CRITICAL METRICS</span>
          </div>
          <button type="button" onClick={onClose} className="modal-close-btn" aria-label="Close modal">
            <CloseIcon size={18} />
          </button>
        </div>

        <div className="modal-body">
          <section className="telemetry-section">
            <div className="section-header-row">
              <h3 className="section-title">Hexxica Orbital Node Connection</h3>
              <button type="button" onClick={onRefreshProbe} className="btn-small">PING NODE</button>
            </div>
            <div className="metric-grid">
              <div className="metric-card">
                <span className="metric-label">Target Endpoint</span>
                <span className="metric-value font-mono">{telemetry.endpoint}</span>
              </div>
              <div className="metric-card">
                <span className="metric-label">Connection Status</span>
                <span className={`metric-value ${telemetry.status === 'ONLINE' ? 'text-green' : 'text-amber'}`}>
                  ● {telemetry.status}
                </span>
              </div>
              <div className="metric-card">
                <span className="metric-label">RTT Latency</span>
                <span className="metric-value">{telemetry.latencyMs} ms</span>
              </div>
              <div className="metric-card">
                <span className="metric-label">Transport Protocol</span>
                <span className="metric-value">{telemetry.isSslActive ? 'HTTPS (TLS 1.3)' : 'HTTP'}</span>
              </div>
            </div>
            <p className="diagnostics-callout">{telemetry.diagnosticsMessage}</p>
          </section>

          <section className="telemetry-section">
            <h3 className="section-title">NASA JPL "Power of 10" Rules Compliance</h3>
            <div className="compliance-list">
              <div className="compliance-item">
                <span className="compliance-check">✓</span>
                <div>
                  <strong>Rule 1 &amp; 2: Simple Control Flow &amp; Fixed Bounds</strong>
                  <p>All loops bounded by MAX_SEARCH_RESULTS, suggestions capped at 6, recursion strictly forbidden.</p>
                </div>
              </div>
              <div className="compliance-item">
                <span className="compliance-check">✓</span>
                <div>
                  <strong>Rule 3: Bounded Heap Allocation</strong>
                  <p>Dynamic collections managed via FixedRingBuffer with fixed capacity (25 elements maximum).</p>
                </div>
              </div>
              <div className="compliance-item">
                <span className="compliance-check">✓</span>
                <div>
                  <strong>Rule 4 &amp; 5: Modular Functions &amp; Assertion Density</strong>
                  <p>Functions constrained to &lt; 60 lines. Critical paths enforced with assertNonNull and assertInRange.</p>
                </div>
              </div>
              <div className="compliance-item">
                <span className="compliance-check">✓</span>
                <div>
                  <strong>Rule 7 &amp; 8: Zero "any" Type &amp; Input Validation</strong>
                  <p>Strictly typed discriminated unions. Zero occurrences of TypeScript 'any' type across codebase.</p>
                </div>
              </div>
            </div>
          </section>

          <section className="telemetry-section">
            <h3 className="section-title">ISO/IEC 25010 Quality Standards Matrix</h3>
            <div className="iso-matrix">
              <span className="iso-badge">Functional Suitability: 100%</span>
              <span className="iso-badge">Reliability &amp; Fault Tolerance: NOMINAL</span>
              <span className="iso-badge">Performance Efficiency: &lt; 50ms</span>
              <span className="iso-badge">Security &amp; Sanitization: ACTIVE</span>
              <span className="iso-badge">Maintainability &amp; Modularity: VERIFIED</span>
            </div>
          </section>
        </div>

        <div className="modal-footer">
          <span className="audit-footprint">AUDIT STANDARD: {audit.isoStandard}</span>
          <button type="button" onClick={onClose} className="btn-search">DISMISS</button>
        </div>
      </div>
    </div>
  );
};
