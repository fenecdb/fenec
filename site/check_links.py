#!/usr/bin/env python3
"""Checks every internal link and anchor in the built site.

    python3 site/check_links.py [site/dist]

Each `href` on every page of `site/dist` that points inside the site -- a
relative path, a root-relative one or a bare `#fragment` -- has to name a
file that was built, and its fragment an `id` on that page. A page removed or
renamed, or a heading whose id changed, otherwise leaves links that answer
404 or land at the top of a page with nothing said: the docs link into each
other hundreds of times, and no one clicks them all after a rename. External
links are not fetched: a build must not depend on the network.

The fragments `docs/fenecql` once held (`moved.json`) are held to the same:
each has to name a page and an id that were built, and none may still be an
id of `docs/fenecql`, whose script would send a reader away from a section
that is there; and `docs/fenecql` has to carry that script, or the map is dead.

Standard library only, as `build.py` is. Exits 1 and lists every broken link.
"""

import html.parser
import json
import os
import sys
import urllib.parse

HERE = os.path.dirname(os.path.abspath(__file__))
MOVED = os.path.join(HERE, "moved.json")


class Page(html.parser.HTMLParser):
    """The ids a page defines and the hrefs it points at."""

    def __init__(self):
        super().__init__(convert_charrefs=True)
        self.ids = set()
        self.hrefs = []
        self.scripts = []
        self.in_script = False

    def handle_endtag(self, tag):
        if tag == "script":
            self.in_script = False

    def handle_data(self, data):
        if self.in_script:
            self.scripts[-1] += data

    def handle_starttag(self, tag, attrs):
        if tag == "script":
            self.in_script = True
            self.scripts.append("")
        for k, v in attrs:
            if k in ("id", "name") and v:
                self.ids.add(v)
            # Only what a reader follows: <link> and <script> are assets,
            # whose absence the build itself would have shown.
            if k == "href" and tag == "a" and v is not None:
                self.hrefs.append((v, self.getpos()[0]))
            # A frame is a page the reader sees: the playground's studio.
            if k == "src" and tag == "iframe" and v is not None:
                self.hrefs.append((v, self.getpos()[0]))


def resolve(dist, page, href):
    """The built file and fragment `href` on `page` names, or None when it
    leaves the site."""
    parts = urllib.parse.urlsplit(href)
    if parts.scheme or parts.netloc or href.startswith("//"):
        return None
    path = urllib.parse.unquote(parts.path)
    if not path:
        target = page
    elif path.startswith("/"):
        target = os.path.join(dist, path.lstrip("/"))
    else:
        target = os.path.join(os.path.dirname(page), path)
    target = os.path.normpath(target)
    # The server serves `/docs/x` as `docs/x.html` and a directory as its
    # index, as Cloudflare's assets do with html_handling's defaults.
    if os.path.isdir(target):
        target = os.path.join(target, "index.html")
    elif not os.path.exists(target) and os.path.exists(target + ".html"):
        target += ".html"
    return target, urllib.parse.unquote(parts.fragment)


def main():
    dist = os.path.abspath(sys.argv[1] if len(sys.argv) > 1 else os.path.join(HERE, "dist"))
    if not os.path.isdir(dist):
        sys.exit(f"{dist} does not exist: run python3 site/build.py first")
    pages = {}
    for root, _, files in os.walk(dist):
        for f in files:
            if f.endswith(".html"):
                p = os.path.join(root, f)
                parser = Page()
                parser.feed(open(p, encoding="utf-8").read())
                pages[os.path.normpath(p)] = parser

    broken, checked = [], 0
    for page, parsed in sorted(pages.items()):
        for href, line in parsed.hrefs:
            if href.startswith(("mailto:", "tel:", "javascript:")):
                continue
            found = resolve(dist, page, href)
            if found is None:
                continue
            checked += 1
            target, fragment = found
            rel = os.path.relpath(page, dist)
            if not os.path.exists(target):
                broken.append(f"{rel}:{line}: {href} -> no such file")
            elif fragment and target.endswith(".html"):
                ids = pages[target].ids if target in pages else set()
                if fragment not in ids:
                    broken.append(f"{rel}:{line}: {href} -> no id `{fragment}`")
    old = pages.get(os.path.join(dist, "docs", "fenecql.html"))
    with open(MOVED, encoding="utf-8") as f:
        moved = json.load(f)
    for fragment, target in sorted(moved.items()):
        checked += 1
        name, _, where = target.partition("#")
        path = os.path.normpath(os.path.join(dist, "docs", name + ".html"))
        if path not in pages:
            broken.append(f"moved.json: {fragment} -> {target}: no such page")
        elif where and where not in pages[path].ids:
            broken.append(f"moved.json: {fragment} -> {target}: no id `{where}`")
        if old is not None and fragment in old.ids:
            broken.append(f"moved.json: {fragment} is an id docs/fenecql still has")
    # The map is read by the one script on docs/fenecql: without it every
    # target above is a page nobody is sent to.
    if old is None or not any("location.replace" in script for script in old.scripts):
        broken.append("moved.json: docs/fenecql carries no script that follows it, so the map is dead")
    for b in broken:
        print(b)
    print(f"{checked} internal links over {len(pages)} pages, {len(broken)} broken")
    sys.exit(1 if broken else 0)


main()
