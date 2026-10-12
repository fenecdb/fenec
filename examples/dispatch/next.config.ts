import type { NextConfig } from 'next';

const config: NextConfig = {
  poweredByHeader: false,
  reactStrictMode: true,
  // The repository has a lockfile of its own above this one.
  outputFileTracingRoot: import.meta.dirname,
  turbopack: { root: import.meta.dirname },
  // `next dev` would write an AGENTS.md and a CLAUDE.md here; the
  // repository's own are at its root.
  agentRules: false,
  async headers() {
    return [
      {
        source: '/:path*',
        headers: [
          { key: 'X-Content-Type-Options', value: 'nosniff' },
          { key: 'Referrer-Policy', value: 'strict-origin-when-cross-origin' },
          { key: 'X-Frame-Options', value: 'DENY' },
        ],
      },
    ];
  },
};

export default config;
