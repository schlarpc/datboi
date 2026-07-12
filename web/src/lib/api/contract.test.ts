/**
 * Contract drift guard (D69). These are TYPE-level assertions: they pin
 * the client's request/response shapes to the GENERATED OpenAPI types
 * (schema.d.ts), so if the rust spec moves and `npm run generate` follows,
 * any client/shim drift fails `npm run check` (svelte-check runs this file
 * through tsc; the expectTypeOf calls are runtime no-ops under vitest).
 */

import { describe, expectTypeOf, test } from 'vitest';
import type { components, operations } from './schema';
import type { EntryState, StateCounts } from '../state';
import * as api from './client';

type Schemas = components['schemas'];

describe('client return types trace to the generated contract', () => {
  test('auth', () => {
    expectTypeOf(api.whoami).returns.resolves.toEqualTypeOf<Schemas['WhoamiResponse']>();
    expectTypeOf(api.login).returns.resolves.toEqualTypeOf<Schemas['SessionResponse']>();
    expectTypeOf(api.acceptInvite).returns.resolves.toEqualTypeOf<Schemas['SessionResponse']>();
    expectTypeOf(api.logout).returns.resolves.toEqualTypeOf<Schemas['OkResponse']>();
  });

  test('read models', () => {
    expectTypeOf(api.systems).returns.resolves.toEqualTypeOf<Schemas['SystemsResponse']>();
    expectTypeOf(api.systemEntries).returns.resolves.toEqualTypeOf<Schemas['EntriesPage']>();
    expectTypeOf(api.entryDetail).returns.resolves.toEqualTypeOf<Schemas['EntryDetail']>();
    expectTypeOf(api.views).returns.resolves.toEqualTypeOf<Schemas['ViewsResponse']>();
    expectTypeOf(api.viewDetail).returns.resolves.toEqualTypeOf<Schemas['ViewDetail']>();
    expectTypeOf(api.viewFiles).returns.resolves.toEqualTypeOf<Schemas['ViewFilesPage']>();
    expectTypeOf(api.storage).returns.resolves.toEqualTypeOf<Schemas['StorageResponse']>();
    expectTypeOf(api.jobs).returns.resolves.toEqualTypeOf<Schemas['JobsResponse']>();
  });

  test('admin', () => {
    expectTypeOf(api.adminUsers).returns.resolves.toEqualTypeOf<Schemas['AdminUsersResponse']>();
    expectTypeOf(api.adminMintInvite).returns.resolves.toEqualTypeOf<
      Schemas['InviteMintResponse']
    >();
    expectTypeOf(api.adminRevokeSessions).returns.resolves.toEqualTypeOf<
      Schemas['SessionsRevokedResponse']
    >();
    expectTypeOf(api.adminGrant).returns.resolves.toEqualTypeOf<Schemas['OkResponse']>();
  });
});

describe('request shapes trace to the generated contract', () => {
  test('list query params come from the operations', () => {
    expectTypeOf<Parameters<typeof api.systemEntries>[1]>().toEqualTypeOf<
      operations['system_entries']['parameters']['query'] | undefined
    >();
    expectTypeOf<Parameters<typeof api.viewFiles>[1]>().toEqualTypeOf<
      operations['view_files']['parameters']['query'] | undefined
    >();
  });

  test('mint-invite params are the InviteMintRequest body', () => {
    expectTypeOf<Parameters<typeof api.adminMintInvite>[0]>().toEqualTypeOf<
      Schemas['InviteMintRequest'] | undefined
    >();
  });

  test("uploadRom's `name` is the ingest_upload query param (XHR transport included)", () => {
    expectTypeOf<Parameters<typeof api.uploadRom>[0]>().toEqualTypeOf<
      operations['ingest_upload']['parameters']['query']['name']
    >();
  });
});

describe('shared vocabularies stay in lockstep', () => {
  test("state.ts' EntryState is exactly the contract's", () => {
    expectTypeOf<EntryState>().toEqualTypeOf<Schemas['EntryState']>();
  });

  test("the contract's Counts satisfies state.ts math", () => {
    // Both directions: a Counts answer feeds completenessPct/barSegments,
    // and the fixtures tests build as StateCounts satisfy the contract.
    expectTypeOf<Schemas['Counts']>().toExtend<StateCounts>();
    expectTypeOf<StateCounts>().toExtend<Schemas['Counts']>();
  });
});
