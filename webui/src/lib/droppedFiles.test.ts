import assert from 'node:assert/strict';
import test from 'node:test';
import { partitionDroppedFiles, readDroppedFilePreview, type DroppedFileLike } from './droppedFiles.ts';

function fakeFile(opts: {
  name: string;
  content?: string;
  size?: number;
  type?: string;
  path?: string;
}): DroppedFileLike {
  const body = opts.content ?? '';
  return {
    name: opts.name,
    size: opts.size ?? body.length,
    type: opts.type ?? 'text/plain',
    path: opts.path,
    text: async () => body,
  };
}

test('partitionDroppedFiles separates OS .path drops from bare WebView2 File drops', () => {
  const withPath = fakeFile({ name: 'main.rs', path: 'C:\\repo\\src\\main.rs' });
  const bare = fakeFile({ name: 'notes.md', content: '# Hi' });
  const blankPath = fakeFile({ name: 'todo.txt', path: '   ', content: 'x' });

  const out = partitionDroppedFiles([withPath, bare, blankPath]);
  assert.deepEqual(out.paths, ['C:\\repo\\src\\main.rs']);
  assert.deepEqual(out.memoryFiles, [bare, blankPath]);
});

test('readDroppedFilePreview reads text files into memory:// preview and rejects binaries/oversize', async () => {
  const md = fakeFile({ name: 'README.md', content: '# Hello world' });
  const preview = await readDroppedFilePreview(md);
  assert.ok(preview);
  assert.match(preview.filePath, /^memory:\/\/[^/]+\/README\.md$/);
  assert.equal(preview.fileName, 'README.md');
  assert.equal(preview.inMemoryContent, '# Hello world');

  const bin = fakeFile({ name: 'archive.zip', content: 'PK\u0003\u0004' });
  assert.equal(await readDroppedFilePreview(bin), null);

  const nulText = fakeFile({ name: 'unknown.dat', content: 'abc\u0000def' });
  assert.equal(await readDroppedFilePreview(nulText), null);

  const huge = fakeFile({ name: 'big.log', content: '', size: 3 * 1024 * 1024 });
  assert.equal(await readDroppedFilePreview(huge), null);
});

test('readDroppedFilePreview loads dropped images and SVGs without hitting /fs/raw', async () => {
  const img = fakeFile({ name: 'shot.png', size: 128, type: 'image/png' });
  const preview = await readDroppedFilePreview(img, async () => 'data:image/png;base64,AAAA');
  assert.ok(preview);
  assert.equal(preview.inMemoryDataUrl, 'data:image/png;base64,AAAA');
  assert.equal(preview.inMemoryContent, undefined);

  const svg = fakeFile({ name: 'icon.svg', content: '<svg><circle r="5"/></svg>', type: 'image/svg+xml' });
  const svgPreview = await readDroppedFilePreview(svg);
  assert.ok(svgPreview);
  assert.equal(svgPreview.inMemoryContent, '<svg><circle r="5"/></svg>');
  assert.equal(
    svgPreview.inMemoryDataUrl,
    'data:image/svg+xml;charset=utf-8,%3Csvg%3E%3Ccircle%20r%3D%225%22%2F%3E%3C%2Fsvg%3E',
  );
});
