import { redirect } from 'next/navigation';
import { current, home } from '../lib/session';

export const dynamic = 'force-dynamic';

export default async function Home() {
  const a = await current();
  redirect(a ? home(a) : '/signin');
}
