// Run against a local ikigai-web server:
// NODE_PATH=/path/to/node_modules node tests/browser/browse-target-size.cjs http://127.0.0.1:8642
// Requires playwright and axe-core. The mounted peer is replaced with a tree fixture;
// the shell, stylesheet, htmx, and shell script are served by ikigai-web itself.
const assert = require('node:assert/strict');
const { chromium } = require('playwright');
const axe = require('axe-core');

(async () => {
  const browser = await chromium.launch({ args: ['--no-sandbox'] });
  try {
    console.log(`Chromium ${browser.version()}, axe-core ${axe.version}`);
    for (const colorScheme of ['light', 'dark']) {
      for (const width of [1280, 320]) {
        const page = await browser.newPage({ bypassCSP: true, colorScheme, viewport: { width, height: 800 } });
        // Matches ikigai-browse's tree_html/explain_button structure. Include adjacent
        // directory controls so axe can detect overlapping target-spacing circles.
        await page.route('**/k/**', route => route.fulfill({
          contentType: 'text/html',
          body: '<ul class="browse-entries">' + Array.from({ length: 7 }, (_, i) =>
            `<li><button class="browse-dir" hx-get="/k/source urn:repo:demo:tree:dir-${i} as=text/html" hx-target="#browse" hx-swap="innerHTML">dir-${i}/</button> <button class="browse-explain-link" title="explain dir-${i}/" hx-get="/k/source urn:repo:demo:explain:dir-${i} as=text/html" hx-target="#browse" hx-swap="innerHTML">?</button></li>`
          ).join('') + '</ul>',
        }));
        await page.goto(`${process.argv[2] || 'http://127.0.0.1:8642'}/browse/urn:repo:demo:tree`);
        await page.locator('.browse-explain-link').last().waitFor();
        // The test context bypasses CSP to inject the audit tool.
        await page.addScriptTag({ content: axe.source });
        const result = await page.evaluate(async () => {
          const sizes = [...document.querySelectorAll('.browse-explain-link')].map(button => {
            const { width, height } = button.getBoundingClientRect();
            return { width, height };
          });
          const audit = await axe.run(document, { runOnly: ['target-size'] });
          return { sizes, violations: audit.violations.map(v => ({ id: v.id, nodes: v.nodes.length })),
            overflow: document.documentElement.scrollWidth > innerWidth };
        });
        console.log(JSON.stringify({ colorScheme, width, ...result }));
        assert.equal(result.violations.length, 0, 'axe target-size violations');
        assert(result.sizes.every(size => size.width >= 24 && size.height >= 24), 'explain targets must be at least 24 by 24 CSS px');
        assert.equal(result.overflow, false, 'tree must fit the viewport');
        // Keyboard focus must remain visible on the native button.
        await page.locator('.browse-explain-link').first().focus();
        assert(await page.locator('.browse-explain-link').first().evaluate(button =>
          button.matches(':focus-visible') && getComputedStyle(button).outlineStyle !== 'none'));
        await page.close();
      }
    }
  } finally {
    await browser.close();
  }
})().catch(error => { console.error(error); process.exitCode = 1; });
