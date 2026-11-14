#!/usr/bin/env python3
"""Check book coverage and every local link in the generated documentation."""

from html.parser import HTMLParser
from pathlib import Path
import re
import sys
from urllib.parse import unquote, urlsplit


ROOT = Path(__file__).resolve().parents[1]
SOURCE = ROOT / 'docs'
OUTPUT = ROOT / 'web/public/docs'


class Page(HTMLParser):
    def __init__(self, content):
        super().__init__()
        self.ids = set()
        self.links = []
        self.feed(content)

    def handle_starttag(self, tag, attrs):
        attrs = dict(attrs)
        if attrs.get('id'):
            self.ids.add(attrs['id'])
        for attribute in ('href', 'src'):
            if attrs.get(attribute):
                self.links.append(attrs[attribute])


def main():
    errors = []
    chapters = {
        (SOURCE / unquote(link)).resolve()
        for link in re.findall(r'\]\(([^)#]+\.md)\)', (SOURCE / 'SUMMARY.md').read_text())
    }
    expected = set(SOURCE.rglob('*.md')) - {SOURCE / 'SUMMARY.md', SOURCE / '404.md'}
    for path in sorted(expected - chapters):
        errors.append(f'{path.relative_to(ROOT)} is missing from SUMMARY.md')
    for path in sorted(chapters - expected):
        errors.append(f'SUMMARY.md references an unknown chapter: {path}')

    pages = {path: Page(path.read_text()) for path in OUTPUT.rglob('*.html')}
    for name in ('index.html', '404.html', 'mermaid.min.js', 'mermaid-init.js', 'searchindex.js'):
        if not (OUTPUT / name).is_file():
            errors.append(f'Missing generated documentation file: {name}')
    for chapter in chapters:
        relative = chapter.relative_to(SOURCE)
        generated = relative.with_suffix('.html')
        if relative.name == 'README.md':
            generated = relative.with_name('index.html')
        if not (OUTPUT / generated).is_file():
            errors.append(f'Missing generated chapter: {generated}')

    checked = 0
    for source, page in pages.items():
        for link in page.links:
            parsed = urlsplit(link)
            if parsed.scheme or parsed.netloc or link == '#':
                continue
            path = unquote(parsed.path)
            if path == '/':
                continue  # Explicit return to the control-plane console.
            if path.startswith('/docs/'):
                target = OUTPUT / path.removeprefix('/docs/')
            elif path.startswith('/'):
                errors.append(f'{source.relative_to(OUTPUT)}: link escapes /docs: {link}')
                continue
            else:
                target = source.parent / path if path else source
            target = target.resolve()
            if target.is_dir():
                target /= 'index.html'
            checked += 1
            if not target.is_relative_to(OUTPUT):
                errors.append(f'{source.relative_to(OUTPUT)}: link escapes the book: {link}')
            elif not target.is_file():
                errors.append(f'{source.relative_to(OUTPUT)}: missing target: {link}')
            elif parsed.fragment and target in pages and unquote(parsed.fragment) not in pages[target].ids:
                errors.append(f'{source.relative_to(OUTPUT)}: missing anchor: {link}')

    if errors:
        print('\n'.join(errors), file=sys.stderr)
        return 1
    print(f'Documentation verified: {len(chapters)} chapters, {len(pages)} HTML pages, {checked} local links and assets.')
    return 0


if __name__ == '__main__':
    sys.exit(main())
