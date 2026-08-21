# Subhost Docs Site

This is a dependency-free static documentation site for the Subhost Web3 workspace.
The content is intentionally split between implemented behavior and design-only material.
Runtime claims should be checked against `crates/*/src`; planning references live under `docs/`.

Serve the repository root locally with:

```bash
python3 -m http.server 4173 --directory .
```

Then open `http://127.0.0.1:4173/docs-site/`.
