import type { MetadataRoute } from 'next';
import { SITE } from '../components/seo';

export default function robots(): MetadataRoute.Robots {
  return {
    rules: [{ userAgent: '*', allow: '/', disallow: ['/api/', '/cart', '/checkout', '/account', '/orders/', '/search'] }],
    sitemap: `${SITE}/sitemap.xml`,
  };
}
