// Minimal React control app for engine-compatibility control experiments.
// When a drop-in browser engine (e.g. obscura) shows app-wide interactivity
// death, serve this (node react-app.mjs, port 4591) and drive it in the SAME
// engine instance: if the button click updates the title here while the real
// app stays inert, the gap is the engine, not the app — switch engines and
// document the evidence instead of debugging app-side event semantics.
import http from 'node:http';
const html = `<!doctype html><html><body><div id=root></div>
<script type="module">
  const React = await import('https://esm.sh/react@19.1.0');
  const { createRoot } = await import('https://esm.sh/react-dom@19.1.0/client');
  let n = 0;
  const App = () => React.createElement('button', { onClick: () => { n++; window.__reactClicked = n; document.title = 'RC' + n; } }, 'reactbtn-' + (window.__mountCount = (window.__mountCount||0)+1));
  createRoot(document.getElementById('root')).render(React.createElement(App));
</script></body></html>`;
http.createServer((req, res) => { res.setHeader('content-type','text/html'); res.end(html); }).listen(4591, '127.0.0.1', () => console.log('mini react app on 4591'));
