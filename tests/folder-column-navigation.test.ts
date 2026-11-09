import assert from 'node:assert/strict';
import test from 'node:test';
import { centerOpenFolder, keepFolderOnBack } from '../src/folder-column-navigation.ts';
import type { ViewState } from '../src/model.ts';

const view: ViewState = {
  path: 'E:\\Work',
  selectedPath: null,
  search: '',
  filter: 'all',
  page: 2,
  scrollTop: 80,
  recentPaths: [],
  previewColumns: [
    { path: 'E:\\Work\\A', page: 1, scrollTop: 20, width: 400 },
    { path: 'E:\\Work\\A\\B', page: 3, scrollTop: 40, width: 420 },
    { path: 'E:\\Work\\A\\B\\C', page: 0, scrollTop: 60 },
  ],
  previewScrollLeft: 180,
  browserColumnWidth: 330,
  revealPath: null,
};

test('Back keeps the current folder and every deeper open preview', () => {
  assert.deepEqual(keepFolderOnBack(view, false), {
    selectedPath: view.path,
    previewColumns: [{ path: view.path, page: 2, scrollTop: 80, width: 330 }, ...view.previewColumns],
  });
  assert.deepEqual(keepFolderOnBack({ ...view, search: 'notes' }, false).previewColumns?.[0],
    { path: view.path, page: 0, scrollTop: 0, width: 330 });
  assert.equal(keepFolderOnBack(view, true).previewColumns?.[0]?.page, 0);
});

test('Center promotes an open folder and keeps only its deeper previews', () => {
  assert.deepEqual(centerOpenFolder(view, 'e:\\WORK\\a\\b'), {
    page: 3, scrollTop: 40, browserColumnWidth: 420, previewColumns: [view.previewColumns[2]],
  });
  assert.deepEqual(centerOpenFolder({ ...view, filter: 'attention' }, 'E:\\Work\\A'), {
    page: 0, scrollTop: 0, browserColumnWidth: 400, previewColumns: view.previewColumns.slice(1),
  });
  assert.deepEqual(centerOpenFolder(view, 'E:\\Other'), {});
});
