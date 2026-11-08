#!/usr/bin/env node
/**
 * Example `[math]` renderer for Yonder — LaTeX → PNG via MathJax + sharp.
 *
 * Contract (see config.toml.example `[math]`):
 *   argv:   <script> [inline|display]   (Yonder appends the mode)
 *   stdin:  LaTeX source, delimiters stripped
 *   stdout: PNG bytes
 *   exit≠0 / timeout / empty stdout → Yonder leaves the formula as text.
 *
 * Dependencies (not vendored — install next to this script):
 *   npm install mathjax-full sharp
 *
 * Configure:
 *   [math]
 *   enabled = true
 *   command = "node /path/to/scripts/math-renderer-example.mjs"
 */
import { mathjax } from 'mathjax-full/js/mathjax.js';
import { TeX } from 'mathjax-full/js/input/tex.js';
import { SVG } from 'mathjax-full/js/output/svg.js';
import { liteAdaptor } from 'mathjax-full/js/adaptors/liteAdaptor.js';
import { RegisterHTMLHandler } from 'mathjax-full/js/handlers/html.js';
import sharp from 'sharp';

const adaptor = liteAdaptor();
RegisterHTMLHandler(adaptor);

const mode = process.argv[2] === 'display' ? 'display' : 'inline';
const latex = await new Promise((resolve, reject) => {
  let data = '';
  process.stdin.setEncoding('utf8');
  process.stdin.on('data', (c) => (data += c));
  process.stdin.on('end', () => resolve(data));
  process.stdin.on('error', reject);
});

if (!latex.trim()) process.exit(1);

try {
  const html = mathjax.document('', {
    InputJax: new TeX({
      packages: ['base', 'ams', 'newcommand', 'noundefined', 'unicode'],
    }),
    OutputJax: new SVG({ fontCache: 'local' }),
  });
  const node = html.convert(latex, { display: mode === 'display' });
  const svg = adaptor.outerHTML(node);
  const png = await sharp(Buffer.from(svg), { density: 300 })
    .resize({ width: 4096, withoutEnlargement: true })
    .png()
    .toBuffer();
  process.stdout.write(png);
} catch (err) {
  console.error(err);
  process.exit(1);
}
