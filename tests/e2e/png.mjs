// Minimal image helpers for the integration suites (no dependencies): PNG encode/decode, and dimension readers
// for JPEG and WebP, so tests can check what the server really produced.
import { deflateSync, inflateSync } from 'node:zlib';

const crcTable = (() => {
  const t = new Uint32Array(256);
  for (let n = 0; n < 256; n++) {
    let c = n;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    t[n] = c >>> 0;
  }
  return t;
})();
function crc32(buf) {
  let c = 0xffffffff;
  for (const b of buf) c = crcTable[(c ^ b) & 0xff] ^ (c >>> 8);
  return (c ^ 0xffffffff) >>> 0;
}
function chunk(type, data) {
  const out = Buffer.alloc(12 + data.length);
  out.writeUInt32BE(data.length, 0);
  out.write(type, 4, 'latin1');
  data.copy(out, 8);
  out.writeUInt32BE(crc32(out.subarray(4, 8 + data.length)), 8 + data.length);
  return out;
}
const SIGNATURE = Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);

/** RGBA PNG from a function (x, y) => [r, g, b, a]. */
export function makePng(w, h, pixel) {
  const raw = Buffer.alloc((w * 4 + 1) * h);
  for (let y = 0; y < h; y++) {
    raw[y * (w * 4 + 1)] = 0; // filter: none
    for (let x = 0; x < w; x++) {
      const p = pixel(x, y);
      const o = y * (w * 4 + 1) + 1 + x * 4;
      raw[o] = p[0];
      raw[o + 1] = p[1];
      raw[o + 2] = p[2];
      raw[o + 3] = p[3] ?? 255;
    }
  }
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(w, 0);
  ihdr.writeUInt32BE(h, 4);
  ihdr[8] = 8; // bit depth
  ihdr[9] = 6; // RGBA
  return Buffer.concat([SIGNATURE, chunk('IHDR', ihdr), chunk('IDAT', deflateSync(raw)), chunk('IEND', Buffer.alloc(0))]);
}

/** red | green over blue | yellow */
export const quadrants = (w, h, alpha = 255) => makePng(w, h, (x, y) => [[255, 0, 0, alpha], [0, 255, 0, alpha], [0, 0, 255, alpha], [255, 255, 0, alpha]][(x < w / 2 ? 0 : 1) + (y < h / 2 ? 0 : 2)]);

/** A PNG header that claims a huge size with no real pixel data behind it (a decompression bomb). */
export function bombPng(w, h) {
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(w, 0);
  ihdr.writeUInt32BE(h, 4);
  ihdr[8] = 8;
  ihdr[9] = 6;
  return Buffer.concat([SIGNATURE, chunk('IHDR', ihdr), chunk('IDAT', deflateSync(Buffer.alloc(16))), chunk('IEND', Buffer.alloc(0))]);
}

/** Decode an 8-bit RGB/RGBA PNG: { width, height, at(x, y) => [r, g, b, a] }. */
export function decodePng(buf) {
  if (!buf.subarray(0, 8).equals(SIGNATURE)) throw new Error('not a PNG');
  let pos = 8, width = 0, height = 0, channels = 0;
  const idat = [];
  while (pos < buf.length) {
    const len = buf.readUInt32BE(pos);
    const type = buf.toString('latin1', pos + 4, pos + 8);
    const data = buf.subarray(pos + 8, pos + 8 + len);
    if (type === 'IHDR') {
      width = data.readUInt32BE(0);
      height = data.readUInt32BE(4);
      if (data[8] !== 8 || ![2, 6].includes(data[9])) throw new Error(`unsupported PNG (depth ${data[8]}, type ${data[9]})`);
      channels = data[9] === 6 ? 4 : 3;
    } else if (type === 'IDAT') idat.push(data);
    pos += 12 + len;
  }
  const raw = inflateSync(Buffer.concat(idat));
  const stride = width * channels;
  const px = Buffer.alloc(stride * height);
  for (let y = 0; y < height; y++) {
    const f = raw[y * (stride + 1)];
    for (let i = 0; i < stride; i++) {
      const v = raw[y * (stride + 1) + 1 + i];
      const a = i >= channels ? px[y * stride + i - channels] : 0;
      const b = y > 0 ? px[(y - 1) * stride + i] : 0;
      const c = y > 0 && i >= channels ? px[(y - 1) * stride + i - channels] : 0;
      let add = 0;
      if (f === 1) add = a;
      else if (f === 2) add = b;
      else if (f === 3) add = (a + b) >> 1;
      else if (f === 4) {
        const p = a + b - c;
        const pa = Math.abs(p - a), pb = Math.abs(p - b), pc = Math.abs(p - c);
        add = pa <= pb && pa <= pc ? a : pb <= pc ? b : c;
      }
      px[y * stride + i] = (v + add) & 0xff;
    }
  }
  return { width, height, channels, at: (x, y) => { const o = y * stride + x * channels; return [px[o], px[o + 1], px[o + 2], channels === 4 ? px[o + 3] : 255]; } };
}

/** Is `pixel` within `tol` of `rgb` (JPEG and resampling blur edges)? */
export const near = (pixel, rgb, tol = 45) => rgb.every((v, i) => Math.abs(pixel[i] - v) <= tol);

/** { width, height } of a JPEG (from the SOF marker). */
export function jpegSize(buf) {
  if (buf[0] !== 0xff || buf[1] !== 0xd8) throw new Error('not a JPEG');
  let pos = 2;
  while (pos < buf.length) {
    if (buf[pos] !== 0xff) throw new Error('bad JPEG marker');
    const marker = buf[pos + 1];
    const len = buf.readUInt16BE(pos + 2);
    if (marker >= 0xc0 && marker <= 0xcf && ![0xc4, 0xc8, 0xcc].includes(marker)) return { height: buf.readUInt16BE(pos + 5), width: buf.readUInt16BE(pos + 7) };
    pos += 2 + len;
  }
  throw new Error('no SOF marker');
}

/** { width, height } of a WebP (VP8L lossless or VP8X extended). */
export function webpSize(buf) {
  if (buf.toString('latin1', 0, 4) !== 'RIFF' || buf.toString('latin1', 8, 12) !== 'WEBP') throw new Error('not a WebP');
  const kind = buf.toString('latin1', 12, 16);
  if (kind === 'VP8X') return { width: 1 + buf.readUIntLE(24, 3), height: 1 + buf.readUIntLE(27, 3) };
  if (kind === 'VP8L') {
    const bits = buf.readUInt32LE(21);
    return { width: (bits & 0x3fff) + 1, height: ((bits >> 14) & 0x3fff) + 1 };
  }
  throw new Error(`unsupported WebP chunk ${kind}`);
}
