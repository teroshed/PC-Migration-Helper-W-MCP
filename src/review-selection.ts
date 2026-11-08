import { isStatus, normalizeReviewPath } from './model.ts';
import type { Document, Entry, GroupTarget, Status, Volume } from './model.ts';

export const SELECTION_LIMIT = 128;
const MAX_REPORTED_PATH_BYTES = 2048;
export const EMPTY_SELECTION: readonly GroupTarget[] = [];
export type SelectionSets = ReadonlyMap<string, readonly GroupTarget[]>;

function object(value: unknown): Record<string, unknown> | null {
  return typeof value === 'object' && value !== null && !Array.isArray(value) ? value as Record<string, unknown> : null;
}

export function rootForSelection(path: string | undefined, volumes: readonly Volume[]): Volume | undefined {
  if (!path) return;
  const lower = path.replaceAll('/', '\\').replace(/\\+$/, '').toLowerCase();
  return volumes.find(volume => {
    const root = volume.root.replace(/\\+$/, '').toLowerCase();
    return volume.available && (lower === root || lower.startsWith(root + '\\'));
  });
}

export function selectionTarget(entry: Pick<Entry, 'path' | 'kind' | 'problem' | 'fingerprint'>, volume: Volume | undefined): GroupTarget | undefined {
  if (!volume || entry.problem || !entry.fingerprint || (entry.kind !== 'file' && entry.kind !== 'folder')) return;
  try {
    const path = normalizeReviewPath(entry.path, [volume.root]);
    const prefix = volume.root.replace(/\\+$/, '');
    const relativePath = path.slice(prefix.length).replace(/^\\+/, '');
    return { path, volumeId: volume.id, relativePath, kind: entry.kind, fingerprint: entry.fingerprint };
  } catch { return; }
}

export function selectionKey(target: Pick<GroupTarget, 'volumeId' | 'relativePath'>): string {
  return target.volumeId + '|' + target.relativePath.toLowerCase();
}

export function selectedFor(sets: SelectionSets, volumeId?: string): readonly GroupTarget[] {
  return volumeId ? sets.get(volumeId) ?? EMPTY_SELECTION : EMPTY_SELECTION;
}

export function replaceSelection(sets: SelectionSets, volumeId: string, targets: readonly GroupTarget[]): SelectionSets {
  const next = new Map(sets);
  if (targets.length) next.set(volumeId, targets);
  else next.delete(volumeId);
  return next;
}

export function changeSelection(current: readonly GroupTarget[], target: GroupTarget, selected?: boolean): readonly GroupTarget[] {
  const key = selectionKey(target);
  const index = current.findIndex(item => selectionKey(item) === key);
  if (selected ?? index < 0) {
    if (index >= 0) {
      const old = current[index]!;
      if (old.path === target.path && old.kind === target.kind && old.fingerprint === target.fingerprint) return current;
      return current.map((item, position) => position === index ? target : item);
    }
    if (current.length >= SELECTION_LIMIT) throw new Error(`Select up to ${SELECTION_LIMIT} entries per review drive.`);
    return [...current, target];
  }
  return index < 0 ? current : current.filter(item => selectionKey(item) !== key);
}

export function selectionSignature(rootId: string | null, targets: readonly GroupTarget[]): string {
  const sorted = [...targets].sort((a, b) => selectionKey(a).localeCompare(selectionKey(b)));
  return JSON.stringify([rootId, sorted.map(target => [selectionKey(target), target.path, target.kind, target.fingerprint])]);
}

export function selectedFoldersWithDescendants(targets: readonly GroupTarget[]): Set<string> {
  const keys = targets.map(selectionKey);
  return new Set(targets.flatMap((target, index) => {
    const key = keys[index]!;
    const prefix = target.relativePath ? key + '\\' : key;
    return target.kind === 'folder' && keys.some(other => other !== key && other.startsWith(prefix)) ? [key] : [];
  }));
}

export function reportSelectionTarget(target: GroupTarget) {
  const encoder = new TextEncoder();
  const pathOmitted = encoder.encode(target.path).length > MAX_REPORTED_PATH_BYTES ||
    encoder.encode(target.relativePath).length > MAX_REPORTED_PATH_BYTES;
  const { path, relativePath, ...identity } = target;
  return { ...identity, path: pathOmitted ? null : path, relativePath: pathOmitted ? null : relativePath, pathOmitted };
}

function parseTarget(value: unknown): GroupTarget | null {
  const target = object(value);
  if (!target) return null;
  const { path, volumeId, relativePath, kind, fingerprint } = target;
  if (typeof path !== 'string' || typeof volumeId !== 'string' || typeof relativePath !== 'string' ||
      (kind !== 'folder' && kind !== 'file') || typeof fingerprint !== 'string' || !fingerprint) return null;
  return { path, volumeId, relativePath, kind, fingerprint };
}

export function loadSelection(value: string | null): SelectionSets {
  if (!value) return new Map();
  try {
    const roots = object(JSON.parse(value));
    if (!roots) return new Map();
    const selections = new Map<string, readonly GroupTarget[]>();
    for (const [root, items] of Object.entries(roots)) {
      if (!Array.isArray(items) || items.length > SELECTION_LIMIT) continue;
      const targets = new Map<string, GroupTarget>();
      for (const raw of items) {
        const target = parseTarget(raw);
        if (target?.volumeId === root) targets.set(selectionKey(target), target);
      }
      if (targets.size) selections.set(root, [...targets.values()]);
    }
    return selections;
  } catch { return new Map(); }
}

export function resolvedSelection(targets: readonly GroupTarget[], volume: Volume): GroupTarget[] {
  return targets.flatMap(target => {
    if (target.volumeId !== volume.id) return [];
    const path = target.relativePath ? volume.root.replace(/\\+$/, '') + '\\' + target.relativePath : volume.root;
    const resolved = selectionTarget({ ...target, path, problem: null }, volume);
    return resolved?.relativePath.toLowerCase() === target.relativePath.toLowerCase() ? [resolved] : [];
  });
}

interface GroupMember { target: GroupTarget; before: Status | null; after: Status | null }
export interface SavedGroup { id: string; memberCount: number; members: GroupMember[] }

function markStatus(value: unknown): Status | null {
  const mark = object(value);
  if (mark?.noteOnly === true) return null;
  const status = mark?.status;
  return isStatus(status) ? status : null;
}

export function savedGroup(value: unknown): SavedGroup | null {
  const event = object(value);
  if (!event || event.type !== 'bulkClassify' || typeof event.id !== 'string' || event.undoneAt != null ||
      !Array.isArray(event.members) || event.members.length < 1 || event.members.length > SELECTION_LIMIT ||
      event.memberCount !== event.members.length) return null;
  const members: GroupMember[] = [];
  for (const raw of event.members) {
    const member = object(raw);
    const target = parseTarget(member?.target);
    if (!member || !target) return null;
    members.push({ target, before: markStatus(member.before), after: markStatus(member.after) });
  }
  return { id: event.id, memberCount: members.length, members };
}

export function latestUndoEvent(document: Document): Record<string, unknown> | undefined {
  for (let index = document.history.length - 1; index >= 0; index--) {
    const event = object(document.history[index]);
    if (event && event.undoneAt == null && (event.type === 'classify' || event.type === 'note' || event.type === 'bulkClassify')) return event;
  }
}
