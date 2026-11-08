import { invoke } from '@tauri-apps/api/core';
import { parseListing } from './model.ts';
import type { Listing } from './model.ts';
import { ReadCache, FolderObservations, folderQueryKey } from './read-cache.ts';
import type { FolderQuery } from './read-cache.ts';

type NativeListingRead = (query: FolderQuery & { refresh: boolean }) => Promise<Listing>;
export class FolderReadCache extends ReadCache<Listing> {
  private observations: FolderObservations;
  private nativeRead: NativeListingRead;
  constructor(nativeRead: NativeListingRead = async query => parseListing(await invoke('list_folder', { ...query })), now = Date.now) {
    super(64, 8 * 1024 * 1024, 15_000, now);
    this.observations = new FolderObservations(now);
    this.nativeRead = nativeRead;
  }
  setSourceScope(scope: string): void { this.observations.setScope(scope); }
  peekListing(query: FolderQuery): Listing | undefined {
    return this.observations.fresh(query.path) ? this.peek(folderQueryKey(query)) : undefined;
  }
  listing(query: FolderQuery, force = false): Promise<Listing> {
    // Explicit refresh invalidates derived rollups and all source observations.
    if (force) { this.clear(); this.observations.clear(); }
    const refresh = !this.observations.fresh(query.path);
    if (refresh) {
      // Discard every cached page/filter of this directory, not an unrelated
      // folder. Keep identical in-flight reads deduplicated.
      this.evictValues(key => JSON.parse(key)[0] === query.path);
    }
    const token = this.observations.token();
    return this.read(folderQueryKey(query), async () => {
      const result = await this.nativeRead({ ...query, refresh });
      if (refresh) this.observations.record(query.path, token);
      return result;
    });
  }
}
