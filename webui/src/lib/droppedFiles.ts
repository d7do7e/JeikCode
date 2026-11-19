export interface DroppedFileLike {
  name: string;
  size: number;
  type?: string;
  path?: string;
  text: () => Promise<string>;
}

export interface PartitionedDroppedFiles<T extends DroppedFileLike> {
  paths: string[];
  memoryFiles: T[];
}

export interface MemoryDroppedPreview {
  filePath: string;
  fileName: string;
  inMemoryContent?: string;
  inMemoryDataUrl?: string;
}

const MEMORY_TEXT_MAX_BYTES = 2 * 1024 * 1024;
const MEMORY_IMAGE_MAX_BYTES = 10 * 1024 * 1024;

const IMAGE_EXTS = new Set(['png', 'jpg', 'jpeg', 'gif', 'webp', 'svg', 'ico', 'bmp', 'avif']);
const NON_TEXT_EXTS = new Set([
  'zip', 'tar', 'gz', 'tgz', '7z', 'rar', 'bz2', 'xz', 'iso',
  'exe', 'dll', 'so', 'dylib', 'bin', 'pdf', 'wasm',
  'mp3', 'wav', 'ogg', 'flac', 'aac',
  'mp4', 'webm', 'mkv', 'avi', 'mov',
  'doc', 'docx', 'xls', 'xlsx', 'ppt', 'pptx',
]);

function fileExt(name: string): string {
  const base = name.split(/[\\/]/).pop() || name;
  const idx = base.lastIndexOf('.');
  return idx >= 0 ? base.slice(idx + 1).toLowerCase() : '';
}

function readFileAsDataUrl(file: Blob): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => resolve(String(reader.result || ''));
    reader.onerror = () => reject(reader.error || new Error('failed to read dropped file'));
    reader.readAsDataURL(file);
  });
}

/**
 * Separate dropped OS `File` objects with real filesystem `.path` from
 * browser/WebView2 `File` drops lacking `.path` so bare filenames are never
 * mistaken for `cwd`-relative disk paths.
 */
export function partitionDroppedFiles<T extends DroppedFileLike>(
  files: ArrayLike<T> | null | undefined,
): PartitionedDroppedFiles<T> {
  const paths: string[] = [];
  const memoryFiles: T[] = [];
  if (!files) return { paths, memoryFiles };
  for (let i = 0; i < files.length; i++) {
    const file = files[i];
    if (!file) continue;
    const realPath = typeof file.path === 'string' ? file.path.trim() : '';
    if (realPath) {
      paths.push(realPath);
    } else if (file.name) {
      memoryFiles.push(file);
    }
  }
  return { paths, memoryFiles };
}

/**
 * Build a unique `memory://` preview payload for a dropped `File` without an OS `.path`.
 * Returns `null` when the file is an unsupported binary or exceeds size bounds.
 */
export async function readDroppedFilePreview(
  file: DroppedFileLike,
  readDataUrl: (f: Blob) => Promise<string> = readFileAsDataUrl,
): Promise<MemoryDroppedPreview | null> {
  const ext = fileExt(file.name);
  const isSvg = ext === 'svg';
  const isImage = IMAGE_EXTS.has(ext) || (file.type || '').startsWith('image/');
  const nonce = `${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 7)}`;
  const filePath = `memory://${nonce}/${file.name}`;

  if (isImage && !isSvg) {
    if (file.size > MEMORY_IMAGE_MAX_BYTES) return null;
    const dataUrl = await readDataUrl(file as unknown as Blob);
    return { filePath, fileName: file.name, inMemoryDataUrl: dataUrl };
  }

  if (NON_TEXT_EXTS.has(ext)) {
    return null;
  }
  if (file.size > MEMORY_TEXT_MAX_BYTES) {
    return null;
  }
  const content = await file.text();
  if (content.includes('\u0000')) {
    return null;
  }
  if (isSvg) {
    return {
      filePath,
      fileName: file.name,
      inMemoryContent: content,
      inMemoryDataUrl: `data:image/svg+xml;charset=utf-8,${encodeURIComponent(content)}`,
    };
  }
  return { filePath, fileName: file.name, inMemoryContent: content };
}
