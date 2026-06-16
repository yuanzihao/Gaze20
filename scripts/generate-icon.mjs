import { deflateSync } from "node:zlib";
import { mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";

const size = 256;
const rgba = Buffer.alloc(size * size * 4);

for (let y = 0; y < size; y += 1) {
  for (let x = 0; x < size; x += 1) {
    const i = (y * size + x) * 4;
    const dx = (x - size / 2) / (size / 2);
    const dy = (y - size / 2) / (size / 2);
    const dist = Math.sqrt(dx * dx + dy * dy);
    const bg = dist < 0.82;
    rgba[i] = bg ? 44 : 0;
    rgba[i + 1] = bg ? 142 : 0;
    rgba[i + 2] = bg ? 118 : 0;
    rgba[i + 3] = bg ? 255 : 0;

    const eye =
      ((x - 128) / 78) ** 2 + ((y - 122) / 42) ** 2 < 1 &&
      Math.abs(y - 122) < 48;
    if (eye) {
      rgba[i] = 248;
      rgba[i + 1] = 255;
      rgba[i + 2] = 252;
      rgba[i + 3] = 255;
    }

    const pupil = (x - 128) ** 2 + (y - 122) ** 2 < 18 ** 2;
    if (pupil) {
      rgba[i] = 22;
      rgba[i + 1] = 58;
      rgba[i + 2] = 50;
      rgba[i + 3] = 255;
    }
  }
}

function crc32(buffer) {
  let crc = ~0;
  for (const byte of buffer) {
    crc ^= byte;
    for (let k = 0; k < 8; k += 1) {
      crc = crc & 1 ? 0xedb88320 ^ (crc >>> 1) : crc >>> 1;
    }
  }
  return ~crc >>> 0;
}

function chunk(type, data) {
  const name = Buffer.from(type);
  const length = Buffer.alloc(4);
  length.writeUInt32BE(data.length);
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(Buffer.concat([name, data])));
  return Buffer.concat([length, name, data, crc]);
}

const scanlines = Buffer.alloc((size * 4 + 1) * size);
for (let y = 0; y < size; y += 1) {
  const rowStart = y * (size * 4 + 1);
  scanlines[rowStart] = 0;
  rgba.copy(scanlines, rowStart + 1, y * size * 4, (y + 1) * size * 4);
}

const ihdr = Buffer.alloc(13);
ihdr.writeUInt32BE(size, 0);
ihdr.writeUInt32BE(size, 4);
ihdr[8] = 8;
ihdr[9] = 6;

const png = Buffer.concat([
  Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
  chunk("IHDR", ihdr),
  chunk("IDAT", deflateSync(scanlines)),
  chunk("IEND", Buffer.alloc(0))
]);

const header = Buffer.alloc(6);
header.writeUInt16LE(0, 0);
header.writeUInt16LE(1, 2);
header.writeUInt16LE(1, 4);

const entry = Buffer.alloc(16);
entry[0] = 0;
entry[1] = 0;
entry[2] = 0;
entry[3] = 0;
entry.writeUInt16LE(1, 4);
entry.writeUInt16LE(32, 6);
entry.writeUInt32LE(png.length, 8);
entry.writeUInt32LE(22, 12);

const iconDir = join(process.cwd(), "src-tauri", "icons");
mkdirSync(iconDir, { recursive: true });
writeFileSync(join(iconDir, "icon.ico"), Buffer.concat([header, entry, png]));
