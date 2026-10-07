/**
 * @file models.ts
 * @description Domain models and strict type contracts for the Hexxica Search Engine.
 * Adheres to ISO/IEC 25010 Quality Model and NASA JPL Rule 8 (Zero Unsafe Coercions / No Any).
 */

export type SearchCategory = 'all' | 'space' | 'tech' | 'news' | 'images';

export type TimeFilter = 'all' | '24h' | '7d' | '30d';

export type NodeStatusType = 'ONLINE' | 'STANDBY' | 'DEGRADED' | 'SIMULATION';

export interface SearchSitelink {
  readonly title: string;
  readonly url: string;
  readonly snippet: string;
}

export interface SearchResult {
  readonly id: string;
  readonly title: string;
  readonly url: string;
  readonly displayUrl: string;
  readonly snippet: string;
  readonly category: SearchCategory;
  readonly score: number;
  readonly publishedAt: string;
  readonly sourceDomain: string;
  readonly tags: readonly string[];
  readonly imageUrl?: string;
  readonly sitelinks?: readonly SearchSitelink[];
  readonly aerospaceTelemetry?: {
    readonly mission: string;
    readonly orbitAltitudeKm?: number;
    readonly groundStation: string;
    readonly signalQuality: string;
  };
}

export interface PeopleAlsoAskItem {
  readonly id: string;
  readonly question: string;
  readonly answer: string;
  readonly sourceTitle: string;
  readonly sourceUrl: string;
}

export interface KnowledgePanelData {
  readonly title: string;
  readonly subtitle: string;
  readonly description: string;
  readonly imageUrl?: string;
  readonly attributes: readonly { readonly label: string; readonly value: string }[];
  readonly officialUrl: string;
}

export interface NodeHealthTelemetry {
  readonly endpoint: string;
  readonly status: NodeStatusType;
  readonly latencyMs: number;
  readonly lastCheckedIso: string;
  readonly httpStatusCode: number;
  readonly isSslActive: boolean;
  readonly diagnosticsMessage: string;
}

export interface SearchExecutionMetrics {
  readonly query: string;
  readonly category: SearchCategory;
  readonly totalMatches: number;
  readonly executionTimeMs: number;
  readonly page: number;
  readonly pageSize: number;
  readonly sourceNode: string;
}

export interface SearchHistoryEntry {
  readonly id: string;
  readonly query: string;
  readonly timestamp: number;
  readonly category: SearchCategory;
}

export interface IsoQualityAudit {
  readonly isoStandard: 'ISO/IEC 25010:2011 & ISO/IEC/IEEE 12207';
  readonly nasaRuleCompliance: '10/10 JPL Rules of Ten';
  readonly zeroAnyTypesEnforced: true;
  readonly boundedLoopsEnforced: true;
  readonly memoryCapBytes: number;
  readonly assertionPassingRate: number;
  readonly targetEndpoint: string;
}
