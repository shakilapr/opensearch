/**
 * @file useHexxicaTelemetry.ts
 * @description Hook managing real-time connectivity telemetry to search.hexxica.space.
 * Zero use of `any`.
 */

import { useEffect, useState } from 'react';
import { NodeHealthTelemetry } from '../domain/models';
import { hexxicaClientInstance } from '../services/hexxicaClient';

const INITIAL_TELEMETRY: NodeHealthTelemetry = {
  endpoint: hexxicaClientInstance.getBaseUrl(),
  status: 'STANDBY',
  latencyMs: 0,
  lastCheckedIso: new Date().toISOString(),
  httpStatusCode: 0,
  isSslActive: hexxicaClientInstance.getBaseUrl().startsWith('https://'),
  diagnosticsMessage: 'Probing the local search API...',
};

export function useHexxicaTelemetry() {
  const [telemetry, setTelemetry] = useState<NodeHealthTelemetry>(INITIAL_TELEMETRY);
  const [isProbing, setIsProbing] = useState<boolean>(false);

  const probe = async () => {
    setIsProbing(true);
    try {
      const result = await hexxicaClientInstance.probeNodeHealth();
      setTelemetry(result);
    } finally {
      setIsProbing(false);
    }
  };

  useEffect(() => {
    // Initial probe
    void probe();

    // Periodic telemetry ping (every 30s)
    const interval = window.setInterval(() => {
      void probe();
    }, 30000);

    return () => {
      window.clearInterval(interval);
    };
  }, []);

  return {
    telemetry,
    isProbing,
    refreshTelemetry: probe,
  };
}
