// Developer-only asset generation. The application embeds the generated files.
// Run with Node.js and sharp installed; no JavaScript runtime is needed to build
// or run OCI Pull. The Lucide geometry lives in assets/brand/container.svg.
const fs = require('node:fs');
const path = require('node:path');
const sharp = require('sharp');
const root = path.resolve(__dirname, '../assets/brand');
const container = fs.readFileSync(path.join(root, 'container.svg'), 'utf8');
const paths = [...container.matchAll(/<path\b[^>]*\/>/g)].map(match => match[0]).join('\n    ');
if (!paths) throw new Error('Lucide Container paths are missing');
const svg = `<svg xmlns="http://www.w3.org/2000/svg" width="512" height="512" viewBox="0 0 512 512">
  <title>OCI Pull</title>
  <rect x="16" y="16" width="480" height="480" rx="104" fill="#171717" stroke="#404040" stroke-width="2"/>
  <g transform="translate(80 80) scale(14.666666667)" fill="none" stroke="#fafafa" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
    ${paths}
  </g>
</svg>
`;
async function main() {
  fs.writeFileSync(path.join(root, 'logo.svg'), svg);
  const render = size => sharp(Buffer.from(svg), { density: 384 }).resize(size, size).png().toBuffer();
  fs.writeFileSync(path.join(root, 'logo.png'), await render(512));
  const sizes = [16, 20, 24, 32, 40, 48, 64, 128, 256];
  const frames = await Promise.all(sizes.map(render));
  const directory = Buffer.alloc(6 + sizes.length * 16);
  directory.writeUInt16LE(1, 2);
  directory.writeUInt16LE(sizes.length, 4);
  let offset = directory.length;
  sizes.forEach((size, index) => {
    const entry = 6 + 16 * index;
    directory[entry] = directory[entry + 1] = size === 256 ? 0 : size;
    directory.writeUInt16LE(1, entry + 4);
    directory.writeUInt16LE(32, entry + 6);
    directory.writeUInt32LE(frames[index].length, entry + 8);
    directory.writeUInt32LE(offset, entry + 12);
    offset += frames[index].length;
  });
  fs.writeFileSync(path.join(root, 'logo.ico'), Buffer.concat([directory, ...frames]));
  const icnsFrames = await Promise.all([
    ['icp4', 16], ['icp5', 32], ['icp6', 64], ['ic07', 128],
    ['ic08', 256], ['ic09', 512], ['ic10', 1024],
  ].map(async ([type, size]) => {
    const png = await render(size);
    const header = Buffer.alloc(8);
    header.write(type, 0, 4, 'ascii');
    header.writeUInt32BE(png.length + 8, 4);
    return Buffer.concat([header, png]);
  }));
  const icnsHeader = Buffer.alloc(8);
  icnsHeader.write('icns', 0, 4, 'ascii');
  icnsHeader.writeUInt32BE(8 + icnsFrames.reduce((sum, frame) => sum + frame.length, 0), 4);
  fs.writeFileSync(path.join(root, 'logo.icns'), Buffer.concat([icnsHeader, ...icnsFrames]));
  console.log(`Generated SVG, PNG, ICO and ICNS`);
}
main().catch(error => { console.error(error); process.exitCode = 1; });
