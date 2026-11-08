import { invoke } from '@tauri-apps/api/core';
import { parseListing } from './model.ts';
import type { Listing } from './model.ts';
import { ReadCache, FolderObservations, StaleReadError, folderQueryKey } from './read-cache.ts';
import type { FolderQuery } from './read-cache.ts';

type NativeListingRead = (query: FolderQuery & { refresh: boolean }) => Promise<Listing>;
export interface FolderReadContext { derived: string; source: string }
interface MetadataRead { queryKey: string; derivedScope: string; promise: Promise<Listing> }
export class FolderReadCache extends ReadCache<Listing> {
  private observations: FolderObservations;
  private nativeRead: NativeListingRead;
  private sourceScope = '';
  private derivedScope = '';
  private metadataPending = new Map<string, MetadataRead>();
  constructor(nativeRead: NativeListingRead = async query => parseListing(await invoke('list_folder', { ...query })), now = Date.now) {
    super(64, 8 * 1024 * 1024, 15_000, now);
    this.observations = new FolderObservations(now);
    this.nativeRead = nativeRead;
  }
  configure(context: FolderReadContext): void {
    this.setScope(context.derived);
    this.setSourceScope(context.source);
  }
  override setScope(scope: string): void {
    this.derivedScope = scope;
    super.setScope(scope);
  }
  setSourceScope(scope: string): void {
    if (scope === this.sourceScope) return;
    this.sourceScope = scope;
    this.observations.setScope(scope);
    this.metadataPending.clear();
    this.clear();
  }
  peekListing(query: FolderQuery): Listing | undefined {
    return this.observations.fresh(query.path) ? this.peek(folderQueryKey(query)) : undefined;
  }
  listing(query: FolderQuery, force = false): Promise<Listing> {
    // Refresh this directory without canceling independent preview columns.
    if (force) {
      this.invalidate(key => JSON.parse(key)[0] === query.path);
      this.observations.invalidate(query.path);
      this.metadataPending.delete(query.path);
    }
    const queryKey = folderQueryKey(query);
    let metadata = this.metadataPending.get(query.path);
    if (!this.observations.fresh(query.path) && !metadata) {
      // TTL expiry must supersede an older derived-only request as well as a
      // cached page. Reserve the metadata read before another caller can join.
      this.invalidate(key => JSON.parse(key)[0] === query.path);
      const token = this.observations.token(query.path);
      const observed = Promise.resolve().then(() => {
        if (!this.observations.current(query.path, token)) throw new StaleReadError();
        return this.nativeRead({ ...query, refresh: true });
      }).then(result => {
        if (!this.observations.current(query.path, token)) throw new StaleReadError();
        this.observations.record(query.path, token);
        return result;
      }).finally(() => {
        if (this.metadataPending.get(query.path)?.promise === observed) this.metadataPending.delete(query.path);
      });
      metadata = { queryKey, derivedScope: this.derivedScope, promise: observed };
      this.metadataPending.set(query.path, metadata);
      // A superseded derived request may never await this shared source read.
      void observed.catch(() => this.observations.discard(query.path, token));
    }
    const sourceRead = metadata;
    const requestScope = this.derivedScope;
    return this.read(queryKey, async () => {
      if (sourceRead) {
        const observed = await sourceRead.promise;
        if (requestScope !== this.derivedScope) throw new StaleReadError();
        if (sourceRead.derivedScope === this.derivedScope && sourceRead.queryKey === queryKey) return observed;
      }
      return this.nativeRead({ ...query, refresh: false });
    });
  }
}
