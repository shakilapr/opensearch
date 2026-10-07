/**
 * @file mockData.ts
 * @description Google-style search engine datasets, knowledge panels, and queries.
 * NASA JPL telemetry records and high-reliability search datasets.
 * Zero use of `any`.
 */

import { KnowledgePanelData, PeopleAlsoAskItem, SearchResult } from './models';

export const MOCK_SEARCH_DATABASE: readonly SearchResult[] = Object.freeze([
  {
    id: 'res-nasa-001',
    title: 'NASA Jet Propulsion Laboratory (JPL) - Space Mission Exploration',
    url: 'https://www.jpl.nasa.gov',
    displayUrl: 'https://www.jpl.nasa.gov',
    snippet: 'The Jet Propulsion Laboratory is a unique national research facility that carries out robotic space and Earth science missions. Exploring Earth, Mars, Jupiter, Europa, and deep space.',
    category: 'space',
    score: 0.99,
    publishedAt: '2026-10-06T12:00:00Z',
    sourceDomain: 'jpl.nasa.gov',
    tags: ['NASA', 'JPL', 'Missions', 'Mars', 'Voyager'],
    sitelinks: [
      {
        title: 'Deep Space Network (DSN) Now',
        url: 'https://eyes.nasa.gov/dsn/dsn.html',
        snippet: 'Real-time telemetry and tracking array for deep space missions including Voyager 1.',
      },
      {
        title: 'Mars 2020 Perseverance Rover',
        url: 'https://mars.nasa.gov/mars2020/',
        snippet: 'Seeking signs of ancient life and collecting samples of rock and regolith.',
      },
      {
        title: 'JPL Missions & Fleet',
        url: 'https://www.jpl.nasa.gov/missions',
        snippet: 'Active, future, and past interplanetary flight missions and telescopes.',
      },
      {
        title: 'Institutional Coding Standards',
        url: 'https://trs.jpl.nasa.gov/handle/2014/40061',
        snippet: 'Ten rules developed by Gerard J. Holzmann for writing safety-critical software.',
      },
    ],
  },
  {
    id: 'res-hexxica-002',
    title: 'Hexxica Space Network | High-Throughput Orbital Edge Search Index',
    url: 'https://search.hexxica.space',
    displayUrl: 'https://search.hexxica.space',
    snippet: 'Autonomous low-latency search index and orbital edge computing node. Provides distributed query caching, real-time telemetry routing, and mission control indexing.',
    category: 'tech',
    score: 0.98,
    publishedAt: '2026-10-07T06:00:00Z',
    sourceDomain: 'search.hexxica.space',
    tags: ['Hexxica', 'Orbital Node', 'Cloud Relay', 'Edge Compute'],
    sitelinks: [
      {
        title: 'Network Telemetry & Node Health',
        url: 'https://search.hexxica.space/network-telemetry',
        snippet: 'Live latency, TLS 1.3 certificate status, and orbital channel lock metrics.',
      },
      {
        title: 'ISO/IEC 25010 Quality Audit',
        url: 'https://search.hexxica.space/iso-audit',
        snippet: 'Certified software product quality model and NASA JPL compliance benchmarks.',
      },
    ],
  },
  {
    id: 'res-nasa-003',
    title: 'JPL Institutional Coding Standard for Safety-Critical Software (The Power of 10)',
    url: 'https://trs.jpl.nasa.gov/handle/2014/40061',
    displayUrl: 'https://trs.jpl.nasa.gov › handle › 2014',
    snippet: 'Ten rules developed by Gerard J. Holzmann for writing safety-critical computational software for spacecraft flight control systems and robotic rovers.',
    category: 'tech',
    score: 0.97,
    publishedAt: '2026-09-15T08:30:00Z',
    sourceDomain: 'jpl.nasa.gov',
    tags: ['NASA Standards', 'Safety-Critical', 'JPL', 'Static Analysis'],
  },
  {
    id: 'res-space-004',
    title: 'James Webb Space Telescope - NASA Science',
    url: 'https://webbtelescope.org',
    displayUrl: 'https://webbtelescope.org',
    snippet: 'The James Webb Space Telescope is the premier observatory of the next decade, studying every phase in the history of our Universe, from the first luminous glows after the Big Bang.',
    category: 'images',
    score: 0.95,
    publishedAt: '2026-10-01T14:20:00Z',
    sourceDomain: 'webbtelescope.org',
    imageUrl: 'https://images.unsplash.com/photo-1614728894747-a83421e2b9c9?auto=format&fit=crop&w=600&q=80',
    tags: ['JWST', 'Deep Space', 'Infrared', 'Astrophysics'],
  },
  {
    id: 'res-news-005',
    title: 'Artemis Program Update: Deep Space Gateway Logistics and Crewed Landings',
    url: 'https://www.nasa.gov/news-release/artemis-lunar-architecture-2026',
    displayUrl: 'https://www.nasa.gov › news-release › artemis-2026',
    snippet: 'NASA confirms synchronized supply modules and autonomous rendezvous navigation for upcoming lunar surface expeditions in the Artemis campaign.',
    category: 'news',
    score: 0.92,
    publishedAt: '2026-10-05T09:15:00Z',
    sourceDomain: 'nasa.gov',
    tags: ['Artemis', 'Lunar', 'Orion', 'SLS', 'News'],
  },
  {
    id: 'res-tech-006',
    title: 'ISO/IEC 25010: Systems and software engineering — Systems and software Quality Requirements and Evaluation (SQuaRE)',
    url: 'https://www.iso.org/standard/35733.html',
    displayUrl: 'https://www.iso.org › standard › 35733',
    snippet: 'ISO/IEC 25010:2011 defines a quality-in-use model composed of five characteristics and a product quality model composed of eight characteristics: functional suitability, performance efficiency, compatibility, usability, reliability, security, maintainability, and portability.',
    category: 'tech',
    score: 0.94,
    publishedAt: '2026-08-11T11:00:00Z',
    sourceDomain: 'iso.org',
    tags: ['ISO Standards', 'Software Quality', 'SQuaRE', 'Reliability'],
  },
  {
    id: 'res-space-007',
    title: 'Voyager 1 - Mission Status | NASA Jet Propulsion Laboratory',
    url: 'https://voyager.jpl.nasa.gov/mission/status/',
    displayUrl: 'https://voyager.jpl.nasa.gov › mission › status',
    snippet: 'Voyager 1 is in interstellar space at approximately 165 AU from Earth. It continues to measure the interstellar medium and send scientific data back to the Deep Space Network.',
    category: 'space',
    score: 0.96,
    publishedAt: '2026-10-06T20:00:00Z',
    sourceDomain: 'jpl.nasa.gov',
    tags: ['Voyager 1', 'Interstellar', 'Heliopause', 'NASA JPL'],
  },
  {
    id: 'res-tech-008',
    title: 'TypeScript 7 Handbook: Advanced Zero-Any Architecture and Static Guarantees',
    url: 'https://www.typescriptlang.org/docs/handbook/intro.html',
    displayUrl: 'https://www.typescriptlang.org › docs › handbook',
    snippet: 'Architectural documentation for mission-critical engineering with TypeScript 7. Complete elimination of dynamic any types in favor of strict discriminated unions, exact optional properties, and static invariants.',
    category: 'tech',
    score: 0.93,
    publishedAt: '2026-09-30T10:10:00Z',
    sourceDomain: 'typescriptlang.org',
    tags: ['TypeScript 7', 'Type Safety', 'No Any', 'Compiler Standards'],
  },
]);

export const MOCK_PEOPLE_ALSO_ASK: readonly PeopleAlsoAskItem[] = Object.freeze([
  {
    id: 'paa-1',
    question: 'What are the 10 rules of NASA coding standards?',
    answer: 'The NASA JPL "Power of 10" rules developed by Gerard Holzmann mandate simple control flow without recursion, fixed loop bounds, bounded heap memory allocation, functions under 60 lines, high assertion density (>= 2 per function), smallest variable scopes, strict return value verification, zero unsafe type coercions, strict pointer/dereference safety, and zero compiler warnings.',
    sourceTitle: 'NASA JPL Safety-Critical Software Guidelines',
    sourceUrl: 'https://trs.jpl.nasa.gov/handle/2014/40061',
  },
  {
    id: 'paa-2',
    question: 'How does search.hexxica.space work?',
    answer: 'search.hexxica.space is a high-reliability orbital search and edge telemetry node providing encrypted TLS 1.3 connectivity, aerospace indexing, and fault-tolerant query execution adhering to ISO/IEC 25010 standards.',
    sourceTitle: 'Hexxica Space Network Architecture',
    sourceUrl: 'https://search.hexxica.space',
  },
  {
    id: 'paa-3',
    question: 'What is ISO/IEC 25010 software quality model?',
    answer: 'ISO/IEC 25010 defines software product quality across 8 key characteristics: Functional Suitability, Performance Efficiency, Compatibility, Usability, Reliability, Security, Maintainability, and Portability.',
    sourceTitle: 'ISO/IEC 25010 SQuaRE Standard',
    sourceUrl: 'https://www.iso.org/standard/35733.html',
  },
  {
    id: 'paa-4',
    question: 'Where is Voyager 1 located right now?',
    answer: 'Voyager 1 is in interstellar space, over 24.8 billion kilometers (165.7 AU) from Earth, communicating with NASA Deep Space Network antennas across California, Spain, and Australia.',
    sourceTitle: 'NASA Eyes on the Solar System - Voyager 1',
    sourceUrl: 'https://voyager.jpl.nasa.gov/mission/status/',
  },
]);

export const MOCK_KNOWLEDGE_PANEL: KnowledgePanelData = Object.freeze({
  title: 'Jet Propulsion Laboratory',
  subtitle: 'Federally funded research and development center in Pasadena, California',
  description: 'The Jet Propulsion Laboratory is a federally funded research and development center managed by the California Institute of Technology for NASA. JPL primary function is the construction and operation of planetary robotic spacecraft.',
  imageUrl: 'https://images.unsplash.com/photo-1614728894747-a83421e2b9c9?auto=format&fit=crop&w=600&q=80',
  officialUrl: 'https://www.jpl.nasa.gov',
  attributes: [
    { label: 'Parent agency', value: 'NASA' },
    { label: 'Location', value: 'Pasadena, California, U.S.' },
    { label: 'Missions', value: 'Voyager, Curiosity, Perseverance, Europa Clipper' },
    { label: 'Operating agency', value: 'California Institute of Technology (Caltech)' },
    { label: 'Coding Standard', value: 'JPL Institutional Rules of Ten' },
  ],
});

export const DEFAULT_SUGGESTIONS: readonly string[] = Object.freeze([
  'NASA JPL Institutional Coding Standards',
  'search.hexxica.space telemetry status',
  'James Webb Space Telescope infrared imagery',
  'Deep Space Network antenna array',
  'ISO/IEC 25010 software quality model',
  'Voyager 1 interstellar plasma telemetry',
  'Mars Perseverance Jezero crater samples',
  'TypeScript 7 zero-any strict architectural patterns',
]);

export const RELATED_SEARCHES: readonly string[] = Object.freeze([
  'nasa coding standards power of 10',
  'search hexxica space network status',
  'iso 25010 quality model summary',
  'jpl software reliability guidelines',
  'deep space network real time telemetry',
  'voyager 1 distance from earth today',
]);
