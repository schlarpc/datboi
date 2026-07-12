import { fireEvent, render, screen } from '@testing-library/svelte';
import { loadLocale } from 'wuchale/load-utils';
import { afterEach, expect, test, vi } from 'vitest';
import '../locales/main.loader.svelte.js';
import type { IngestReport, JobDetailBody, MatchedEntry } from '../lib/api/types';
import { calledPath, installFetch, installUploadXhr } from '../test/mock-api';
import Ingest from './Ingest.svelte';

await loadLocale('en');

afterEach(() => vi.unstubAllGlobals());

const emptyReport: IngestReport = {
  files_scanned: 0,
  files_unchanged: 0,
  files_stored: 0,
  files_already_present: 0,
  chd_v5: 0,
  members_claimed: 0,
  members_extracted: 0,
  detector_hits: 0,
  skipper_skipped_large: 0,
  dats_imported: [],
  errors: [],
  member_skips: [],
  notes: [],
};

function doneJob(
  report: Partial<IngestReport>,
  matched: MatchedEntry[] = [],
  matchedTotal = matched.length,
): JobDetailBody {
  return {
    id: 1,
    name: 'ingest — 2 files',
    progress: 100,
    kind: 'ingest',
    state: 'done',
    files_total: 2,
    files_done: 2,
    bytes_total: 8,
    bytes_done: 8,
    started_at: 1000,
    finished_at: 1002,
    report: { ...emptyReport, ...report },
    matched,
    matched_total: matchedTotal,
    error: null,
  };
}

function pickFiles(files: File[]): Promise<void> {
  const input = document.querySelectorAll<HTMLInputElement>('input[type="file"]')[0];
  expect(input).toBeTruthy();
  return fireEvent.change(input, { target: { files } }) as unknown as Promise<void>;
}

test('pick → upload → auto-ingest → report card', async () => {
  installFetch({
    jobTimeline: [
      doneJob(
        {
          files_scanned: 2,
          files_stored: 1,
          files_already_present: 1,
          members_claimed: 3,
        },
        [
          { name: 'Mario Kart DS (USA, Australia)', source: 'no-intro/nds' },
          { name: 'Advance Wars (USA)', source: 'no-intro/gba' },
        ],
        3, // one more lit up than the (test-sized) cap carried
      ),
    ],
  });
  const sent = installUploadXhr();
  render(Ingest);

  await pickFiles([new File(['aaaa'], 'alpha.gba'), new File(['zzzz'], 'pack.zip')]);

  // Both files were uploaded with their names.
  expect(await screen.findByText('alpha.gba')).toBeTruthy();
  expect(screen.getByText('pack.zip')).toBeTruthy();
  expect(sent.map((s) => s.name)).toEqual(['alpha.gba', 'pack.zip']);

  // The job finished immediately (scripted), so the report renders.
  expect(await screen.findByText(/new blob/)).toBeTruthy(); // 1 stored: singular
  expect(screen.getByText(/dupes/)).toBeTruthy();
  expect(screen.getByText(/archive members/)).toBeTruthy();
  expect(screen.getByText('refused')).toBeTruthy();
  expect(screen.getByText(/3.*claimed in place/)).toBeTruthy();

  // The matched section names the newly satisfied entries and owns up
  // to the capped tail.
  expect(screen.getByText('matched')).toBeTruthy();
  expect(screen.getByText('Mario Kart DS (USA, Australia)')).toBeTruthy();
  expect(screen.getByText('no-intro/nds')).toBeTruthy();
  expect(screen.getByText('Advance Wars (USA)')).toBeTruthy();
  expect(screen.getByText(/and 1 more/)).toBeTruthy();
});

test('the flow survives leaving and returning — the report is still there', async () => {
  installFetch({ jobTimeline: [doneJob({ files_scanned: 1, files_stored: 1 })] });
  installUploadXhr();
  const first = render(Ingest);
  await pickFiles([new File(['x'], 'come-back.gba')]);
  expect(await screen.findByText('REPORT')).toBeTruthy();

  // Navigate away (unmount) and back (fresh mount): the flow is app
  // state, so the report — not a pristine dropzone — greets the user.
  first.unmount();
  render(Ingest);
  expect(await screen.findByText('REPORT')).toBeTruthy();
});

test('refusals list per-file reasons from the report', async () => {
  installFetch({
    jobTimeline: [
      doneJob({
        files_scanned: 1,
        errors: [{ path: 'bad.zip', error: 'central directory lies' }],
        member_skips: [{ path: 'bad.zip', member: 'x.bin', reason: 'zip64 member' }],
      }),
    ],
  });
  installUploadXhr();
  render(Ingest);

  await pickFiles([new File(['zzzz'], 'bad.zip')]);

  expect(await screen.findByText('central directory lies')).toBeTruthy();
  expect(screen.getByText('bad.zip :: x.bin')).toBeTruthy();
  expect(screen.getByText('zip64 member')).toBeTruthy();
});

test('the dats lane renders imported dats with their resolved identity', async () => {
  installFetch({
    jobTimeline: [
      doneJob({
        files_scanned: 1,
        files_stored: 1,
        dats_imported: [
          { path: 'nds.zip', provider: 'no-intro', system: 'nds', entries: 5000 },
        ],
      }),
    ],
  });
  installUploadXhr();
  render(Ingest);

  await pickFiles([new File(['aaaa'], 'alpha.gba'), new File(['zzzz'], 'nds.zip')]);

  expect(await screen.findByText(/dat imported/)).toBeTruthy(); // 1 dat: singular
  expect(screen.getByText('nds.zip')).toBeTruthy();
  expect(screen.getByText('no-intro/nds — 5,000 entries')).toBeTruthy();
});

test('a failed upload is reported without starting a job', async () => {
  const handler = installFetch({});
  installUploadXhr({ fail: true });
  render(Ingest);

  await pickFiles([new File(['aaaa'], 'alpha.gba')]);

  expect(await screen.findByText('induced upload failure')).toBeTruthy();
  const starts = handler.mock.calls.filter(([input]) => calledPath(input) === '/v1/ingest');
  expect(starts.length).toBe(0);
});

test('a refused ingest start surfaces as the failure line', async () => {
  installFetch({ ingestFail: true });
  installUploadXhr();
  render(Ingest);

  await pickFiles([new File(['aaaa'], 'alpha.gba')]);

  expect(await screen.findByText(/something went wrong/)).toBeTruthy();
  expect(screen.getByText(/unknown or expired upload/)).toBeTruthy();
});
