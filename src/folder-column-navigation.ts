import type { PreviewColumn, ViewState } from './model.ts';

// Match the path identity used by the review strip, including drive root spelling.
function pathKey(path: string): string {
  return path.replace(/\\$/, '').toLowerCase();
}

export function centerOpenFolder(view: ViewState, path: string): Partial<ViewState> {
  const index = view.previewColumns.findIndex(column => pathKey(column.path) === pathKey(path));
  const column = view.previewColumns[index];
  if (!column) return {};

  const sameQuery = view.filter === 'all';
  return {
    page: sameQuery ? column.page : 0,
    scrollTop: sameQuery ? column.scrollTop : 0,
    browserColumnWidth: column.width ?? null,
    previewColumns: view.previewColumns.slice(index + 1),
  };
}

export function keepFolderOnBack(view: ViewState, showingKnownFolders: boolean): Partial<ViewState> {
  const sameQuery = !showingKnownFolders && view.search === '' && view.filter === 'all';
  const previousRoot: PreviewColumn = {
    path: view.path,
    page: sameQuery ? view.page : 0,
    scrollTop: sameQuery ? view.scrollTop : 0,
    width: view.browserColumnWidth ?? undefined,
  };
  // The saved-view validator bounds the depth. Preserve the closest previews
  // if an existing saved branch is already at that limit.
  return { selectedPath: view.path, previewColumns: [previousRoot, ...view.previewColumns.slice(0, 4095)] };
}
