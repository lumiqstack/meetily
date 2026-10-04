# Frontend tests

## Unit tests (default)

```bash
pnpm run test
```

Runs `bun test`, which discovers `*.test.*` files under `tests/`. It needs only
the installed project dependencies and no browser, so its result is
deterministic on any machine.

## Browser fixtures (explicit)

```bash
pnpm run test:browser
```

Runs `tests/browser/teams-recap-dom.browser.cjs` with `node --test`. It drives
the Teams recap extractor (`src-tauri/src/audio/teams_recap.js`) against
synthetic local pages in a real browser, so it needs:

- **Playwright** as a Node module. It is intentionally not a project dependency;
  point `NODE_PATH` at an existing install, for example
  `NODE_PATH=/path/to/node_modules pnpm run test:browser`.
- **Google Chrome** installed (Playwright's `chrome` channel).

If either is missing the suite fails with an explicit message rather than
skipping. Run it whenever the recap extraction script changes, and report it
separately from the unit tests: an unavailable browser run means recap DOM
coverage is unverified, not passed.
