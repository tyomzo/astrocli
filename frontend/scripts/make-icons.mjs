/*
 * Generates the PWA icon set — USB-09.
 *
 * The PNGs are committed, so this is not part of the build; it is here so the icons are
 * reproducible from source rather than being three binaries nobody can regenerate or explain.
 * Run it with `npm run icons` after changing the artwork.
 *
 * It encodes PNG by hand (zlib is in Node's standard library, the rest is a header and a CRC)
 * because the alternative is a native image dependency in the toolchain of a project whose build
 * has to work on a Pi. Roughly sixty lines, run once a year.
 *
 * The colours here are literals, and this file is not a component: an app icon is baked into a
 * bitmap the launcher draws long before any stylesheet exists, so it cannot resolve a token. It
 * is drawn in the night-mode accent hue deliberately — the icon is looked at in the dark, next to
 * the launcher's other icons, by someone whose eyes are adapted.
 */

import { mkdirSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { deflateSync } from 'node:zlib';

const OUT = join(dirname(fileURLToPath(import.meta.url)), '..', 'public', 'icons');

const BACKGROUND = [0x00, 0x00, 0x00];
const STAR = [0xff, 0xe8, 0xdc];

/*
 * One variant per deployment kind, distinguished by hue.
 *
 * This is a safety measure, not decoration. A dev node and a production node install as separate
 * PWAs (separate origins), which means two icons sit side by side on one home screen — and one of
 * them drives a real mount with a camera bolted to it. Telling them apart has to survive a glance
 * in the dark, so the difference is hue at full strength rather than a badge or a shade.
 *
 * Deliberately not red-versus-green: that pair fails for roughly 8% of men and collapses under
 * night mode. Warm amber against cold cyan survives both.
 */
const VARIANTS = [
  { suffix: '', reticle: [0xff, 0x5a, 0x3c] },
  { suffix: '-dev', reticle: [0x3c, 0xc8, 0xff] },
];

/** Supersampling factor. 4×4 subsamples per pixel is enough for a ring at 192 px. */
const SS = 4;

/**
 * Draw the reticle.
 *
 * `reach` is the radius the artwork extends to, as a fraction of the icon's width. A maskable
 * icon must keep everything meaningful inside a circle of 80% diameter — the launcher may crop
 * to any shape inside that — so it gets 0.40 and the plain icon gets 0.47.
 */
function render(size, reach, RETICLE) {
  const pixels = Buffer.alloc(size * size * 4);
  const art = reach * size;

  const ringRadius = 0.62 * art;
  const ringHalfWidth = 0.055 * art;
  const tickInner = ringRadius + ringHalfWidth + 0.1 * art;
  const tickOuter = art;
  const tickHalfWidth = 0.045 * art;
  const starRadius = 0.12 * art;

  const centre = size / 2;

  for (let y = 0; y < size; y += 1) {
    for (let x = 0; x < size; x += 1) {
      let reticle = 0;
      let star = 0;

      for (let sy = 0; sy < SS; sy += 1) {
        for (let sx = 0; sx < SS; sx += 1) {
          const px = x + (sx + 0.5) / SS - centre;
          const py = y + (sy + 0.5) / SS - centre;
          const r = Math.hypot(px, py);

          if (r <= starRadius) {
            star += 1;
          } else if (Math.abs(r - ringRadius) <= ringHalfWidth) {
            reticle += 1;
          } else if (r >= tickInner && r <= tickOuter) {
            // Four ticks on the axes, which read as a crosshair without crossing the ring.
            if (Math.abs(px) <= tickHalfWidth || Math.abs(py) <= tickHalfWidth) {
              reticle += 1;
            }
          }
        }
      }

      const total = SS * SS;
      const offset = (y * size + x) * 4;
      const colour = blend(
        blend(BACKGROUND, RETICLE, reticle / total),
        STAR,
        star / total,
      );
      pixels[offset] = colour[0];
      pixels[offset + 1] = colour[1];
      pixels[offset + 2] = colour[2];
      pixels[offset + 3] = 0xff;
    }
  }

  return pixels;
}

function blend(under, over, alpha) {
  return [0, 1, 2].map((i) => Math.round(under[i] * (1 - alpha) + over[i] * alpha));
}

// --- PNG encoding ------------------------------------------------------------------------

const CRC_TABLE = (() => {
  const table = new Int32Array(256);
  for (let n = 0; n < 256; n += 1) {
    let c = n;
    for (let k = 0; k < 8; k += 1) {
      c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    }
    table[n] = c;
  }
  return table;
})();

function crc32(buffer) {
  let crc = -1;
  for (const byte of buffer) {
    crc = CRC_TABLE[(crc ^ byte) & 0xff] ^ (crc >>> 8);
  }
  return (crc ^ -1) >>> 0;
}

function chunk(type, data) {
  const length = Buffer.alloc(4);
  length.writeUInt32BE(data.length);
  const body = Buffer.concat([Buffer.from(type, 'ascii'), data]);
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(body));
  return Buffer.concat([length, body, crc]);
}

function png(size, pixels) {
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(size, 0);
  ihdr.writeUInt32BE(size, 4);
  ihdr[8] = 8; // bit depth
  ihdr[9] = 6; // colour type: RGBA
  // compression, filter and interlace methods: the only values PNG defines.

  // One scanline per row, each prefixed with filter type 0 (none). Filtering would compress
  // better; at these sizes the difference is under a kilobyte and this stays readable.
  const stride = size * 4;
  const raw = Buffer.alloc((stride + 1) * size);
  for (let y = 0; y < size; y += 1) {
    pixels.copy(raw, y * (stride + 1) + 1, y * stride, (y + 1) * stride);
  }

  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk('IHDR', ihdr),
    chunk('IDAT', deflateSync(raw, { level: 9 })),
    chunk('IEND', Buffer.alloc(0)),
  ]);
}

// --- output ------------------------------------------------------------------------------

mkdirSync(OUT, { recursive: true });

for (const { suffix, reticle } of VARIANTS) {
  for (const [name, size, reach] of [
    [`icon${suffix}-192.png`, 192, 0.47],
    [`icon${suffix}-512.png`, 512, 0.47],
    [`icon${suffix}-maskable-512.png`, 512, 0.4],
  ]) {
    const file = join(OUT, name);
    writeFileSync(file, png(size, render(size, reach, reticle)));
    console.log(`wrote ${file}`);
  }
}
