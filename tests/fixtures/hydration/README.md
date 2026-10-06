# Hydration fixtures

Production builds of frameworks, used by the hydration benchmark in
`crates/catpaw-bindings-boa/tests/hydration.rs`, which renders a small
application on the server side, hydrates it in a page and times the run.

| File | Package | License |
|---|---|---|
| `react.production.min.js`, `react-dom.production.min.js`, `react-dom-server-legacy.browser.production.min.js` | react, react-dom 18.3.1 | MIT |
| `vue.global.prod.js` | vue 3.5.22 | MIT |

The files are unmodified copies from the npm packages; their license
headers are inside.
