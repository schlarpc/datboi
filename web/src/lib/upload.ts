/**
 * Drop/picker payload collection for ingest: files AND folders, with
 * client-relative names — the names the server's report will wear.
 * Pure functions so the traversal is unit-testable without a browser.
 */

export interface DropFile {
  /** Client-relative path, `/`-separated (e.g. `roms/pack.zip`). */
  name: string;
  file: File;
}

/**
 * Everything under a drop, folders included. Entries MUST be grabbed
 * synchronously — the DataTransferItemList is dead once the drop
 * handler yields — so the traversal awaits only after collecting them.
 * Browsers without webkitGetAsEntry fall back to the flat file list
 * (no folder support there; files still work).
 */
export function collectDrop(dt: DataTransfer): Promise<DropFile[]> {
  const entries: FileSystemEntry[] = [];
  let supported = false;
  for (const item of Array.from(dt.items ?? [])) {
    if (typeof item.webkitGetAsEntry === 'function') {
      supported = true;
      const entry = item.webkitGetAsEntry();
      if (entry) entries.push(entry);
    }
  }
  if (!supported) {
    return Promise.resolve(Array.from(dt.files).map((file) => ({ name: file.name, file })));
  }
  return (async () => {
    const out: DropFile[] = [];
    for (const entry of entries) {
      await walk(entry, out);
    }
    return out;
  })();
}

async function walk(entry: FileSystemEntry, out: DropFile[]): Promise<void> {
  if (entry.isFile) {
    const file = await new Promise<File>((resolve, reject) => {
      (entry as FileSystemFileEntry).file(resolve, reject);
    });
    out.push({ name: entry.fullPath.replace(/^\//, ''), file });
    return;
  }
  if (entry.isDirectory) {
    const reader = (entry as FileSystemDirectoryEntry).createReader();
    // readEntries answers AT MOST ~100 entries per call (Chrome); a
    // single call silently truncates big folders. Loop until empty.
    for (;;) {
      const batch = await new Promise<FileSystemEntry[]>((resolve, reject) => {
        reader.readEntries(resolve, reject);
      });
      if (batch.length === 0) {
        break;
      }
      for (const child of batch) {
        await walk(child, out);
      }
    }
  }
}

/**
 * Picker payloads: a folder picker (`webkitdirectory`) gives relative
 * paths, a plain file picker only leaf names.
 */
export function pickedFiles(list: FileList): DropFile[] {
  return Array.from(list).map((file) => ({
    name: file.webkitRelativePath || file.name,
    file,
  }));
}
