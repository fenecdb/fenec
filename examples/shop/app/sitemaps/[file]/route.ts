import { file, xml } from '../../../lib/sitemap';

export const revalidate = 3600;

export async function GET(_req: Request, { params }: { params: Promise<{ file: string }> }) {
  const name = (await params).file.replace(/\.xml$/, '');
  const body = await file(name);
  return body ? xml(body) : new Response('not found', { status: 404 });
}
