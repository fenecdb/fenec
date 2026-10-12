'use client';
// The page's own way to the router: a five-minute token the server minted
// for the person, swapped for a fresh one every two minutes. A token
// swapped is a connection of its own, so every stream opened on it reopens.
import { useEffect, useMemo, useState } from 'react';
import { connect, type FenecHttp } from '@fenecdb/web/client';

export interface Access {
  token: string;
  url: string;
  expires: number;
}

export function useConnection(initial: Access): FenecHttp {
  const [access, setAccess] = useState(initial);
  useEffect(() => {
    const every = setInterval(async () => {
      const res = await fetch('/api/token', { cache: 'no-store' });
      if (res.ok) setAccess(await res.json());
      // Signed out meanwhile: the page itself sends them to sign in.
      else if (res.status === 401) location.reload();
    }, 120_000);
    return () => clearInterval(every);
  }, []);
  return useMemo(() => connect(access.url, { token: access.token }), [access]);
}

/** A clock that ticks every `ms`, for waiting times. */
export function useNow(ms = 1000): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const t = setInterval(() => setNow(Date.now()), ms);
    return () => clearInterval(t);
  }, [ms]);
  return now;
}

/** Minutes and seconds, `4:05`, or hours and minutes past an hour. */
export function since(ms: number): string {
  const s = Math.max(0, Math.floor(ms / 1000));
  if (s >= 3600) return `${Math.floor(s / 3600)} h ${Math.floor((s % 3600) / 60)} min`;
  return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`;
}
