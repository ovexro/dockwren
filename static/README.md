Vendored assets (served from the binary, no CDN at runtime).

- `dockwren.css`: our own stylesheet (system font stack, light/dark via prefers-color-scheme).
- `htmx.min.js`: htmx 2.0.4 from https://cdn.jsdelivr.net/npm/htmx.org@2.0.4/dist/htmx.min.js
  (sha256 e209dda5c8235479f3166defc7750e1dbcd5a5c1808b7792fc2e6733768fb447)
- `xterm.js`, `xterm.css`: xterm.js 5.5.0 (MIT) from https://cdn.jsdelivr.net/npm/@xterm/xterm@5.5.0/ (lib/xterm.js, css/xterm.css)
  (sha256 xterm.js 1f991ac3b4b283ebf96e60ae23a00a52765dd3a2e46fa6fdda9f1aab032f7495, xterm.css ba8e6985669488981ccf40c0cefe3aba80722cb6c92de7ad628b0bd717faf2b6)
- `addon-fit.js`: @xterm/addon-fit 0.10.0 (MIT) from https://cdn.jsdelivr.net/npm/@xterm/addon-fit@0.10.0/lib/addon-fit.js
  (sha256 bdaefa370b1bfc42ee88d46fe6072400902a4d4b2d45cd93438dda9b23c97089)
- `terminal.js`: our own web-terminal client (only served when `DOCKWREN_TERMINAL=1`).
