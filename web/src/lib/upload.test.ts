import { expect, test } from 'vitest';
import { collectDrop, pickedFiles } from './upload';

function fileEntry(fullPath: string): FileSystemEntry {
  const leaf = fullPath.split('/').pop() ?? fullPath;
  return {
    isFile: true,
    isDirectory: false,
    fullPath,
    file: (cb: (f: File) => void) => cb(new File(['x'], leaf)),
  } as unknown as FileSystemEntry;
}

/** Directory whose reader batches like Chrome: ≤100 entries per call. */
function dirEntry(fullPath: string, children: FileSystemEntry[]): FileSystemEntry {
  return {
    isFile: false,
    isDirectory: true,
    fullPath,
    createReader: () => {
      let offset = 0;
      return {
        readEntries: (cb: (batch: FileSystemEntry[]) => void) => {
          const batch = children.slice(offset, offset + 100);
          offset += batch.length;
          cb(batch);
        },
      };
    },
  } as unknown as FileSystemEntry;
}

function dropOf(entries: FileSystemEntry[]): DataTransfer {
  return {
    items: entries.map((entry) => ({ webkitGetAsEntry: () => entry })),
    files: [],
  } as unknown as DataTransfer;
}

test('a loose file drop keeps its leaf name', async () => {
  const files = await collectDrop(dropOf([fileEntry('/alpha.gba')]));
  expect(files.map((f) => f.name)).toEqual(['alpha.gba']);
});

test('a folder drop recurses and keeps relative paths', async () => {
  const drop = dropOf([
    dirEntry('/roms', [
      fileEntry('/roms/alpha.gba'),
      dirEntry('/roms/discs', [fileEntry('/roms/discs/beta.bin')]),
    ]),
    fileEntry('/loose.zip'),
  ]);
  const files = await collectDrop(drop);
  expect(files.map((f) => f.name)).toEqual(['roms/alpha.gba', 'roms/discs/beta.bin', 'loose.zip']);
});

test('big folders survive the 100-entry readEntries batching', async () => {
  const children = Array.from({ length: 250 }, (_, i) => fileEntry(`/big/f${i}.bin`));
  const files = await collectDrop(dropOf([dirEntry('/big', children)]));
  expect(files.length).toBe(250);
});

test('browsers without webkitGetAsEntry fall back to the flat list', async () => {
  const drop = {
    items: [{}],
    files: [new File(['x'], 'plain.gba')],
  } as unknown as DataTransfer;
  const files = await collectDrop(drop);
  expect(files.map((f) => f.name)).toEqual(['plain.gba']);
});

test('picker names: folder pickers give relative paths, file pickers leaves', () => {
  const flat = new File(['x'], 'alpha.gba');
  const nested = new File(['x'], 'beta.bin');
  Object.defineProperty(nested, 'webkitRelativePath', { value: 'roms/discs/beta.bin' });
  const files = pickedFiles([flat, nested] as unknown as FileList);
  expect(files.map((f) => f.name)).toEqual(['alpha.gba', 'roms/discs/beta.bin']);
});
