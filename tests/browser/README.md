# Browse pointer targets

This optional browser regression probe runs against a real `ikigai-web` shell. It replaces
only `/k/` responses with a seven-directory fixture matching `ikigai-browse`'s
`tree_html` / `explain_button` structure. It tests the stylesheet served by the server,
axe's `target-size` rule, explain-button dimensions, narrow-screen overflow, and the
native focus outline. It does not audit all browse faces or all accessibility rules.
The Rust HTTP suite pins the minimum dimensions in the served CSS in CI.

Install the probe's tools in a local directory (Node.js is required):

```sh
npm install --prefix .local-tools playwright@1.56.1 axe-core@4.10.3
.local-tools/node_modules/.bin/playwright install chromium
```

Start the server. A disconnected mount is sufficient because the probe intercepts the
peer responses; use an empty config to avoid loading personal mounts:

```sh
mkdir -p .local-tools
printf '' > .local-tools/config.toml
cargo run --locked -- --config .local-tools/config.toml \
  --mount 'prefer urn:repo:=/nonexistent.sock' --port 18642
```

In another terminal:

```sh
NODE_PATH="$PWD/.local-tools/node_modules" node tests/browser/browse-target-size.cjs http://127.0.0.1:18642
```

The probe prints browser and axe versions, button sizes, violations, and overflow for
light and dark schemes at 1280 and 320 CSS px, and exits unsuccessfully on a regression.
It bypasses CSP in the test context only to inject axe. Manual keyboard, zoom, and
assistive technology checks are separate and have not been performed by this probe.
