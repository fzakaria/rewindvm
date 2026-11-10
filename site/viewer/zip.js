// The .rwd file inside a zip. GitHub does not take an attachment named
// .rwd, so `rewind bugreport` has people attach their export zipped, and
// app.js opens such a zip as the export it holds.
//
// Only what that needs of the format is read: the end record, the central
// directory it points at, and one member, stored or deflated. The browser
// inflates the member.

// The records' signatures, read as little-endian 32-bit numbers: a
// member's local header, its entry in the central directory, and the end
// record that closes the archive.
const LOCAL_SIGNATURE = 0x04034b50;
const CENTRAL_SIGNATURE = 0x02014b50;
const END_SIGNATURE = 0x06054b50;

// The size of each record before its variable parts, and the longest
// comment that can follow the end record.
const LOCAL_SIZE = 30;
const CENTRAL_SIZE = 46;
const END_SIZE = 22;
const END_COMMENT_MAX = 0xffff;

// Where each record keeps the fields read here.
const END_ENTRIES = 10;
const END_DIRECTORY_OFFSET = 16;
const CENTRAL_FLAGS = 8;
const CENTRAL_METHOD = 10;
const CENTRAL_PACKED_SIZE = 20;
const CENTRAL_SIZE_FIELD = 24;
const CENTRAL_NAME_LENGTH = 28;
const CENTRAL_EXTRA_LENGTH = 30;
const CENTRAL_COMMENT_LENGTH = 32;
const CENTRAL_LOCAL_OFFSET = 42;
const LOCAL_NAME_LENGTH = 26;
const LOCAL_EXTRA_LENGTH = 28;

// The two ways a member is packed that are read here, and the name the
// browser's inflater knows the second by.
const METHOD_STORED = 0;
const METHOD_DEFLATE = 8;
const DEFLATE_RAW = "deflate-raw";

// The flag bit of a member encrypted with a password.
const FLAG_ENCRYPTED = 1;

// What a field holds when its real value is in a zip64 record, which an
// archive needs only past 4 GiB or 65,535 members.
const ZIP64_COUNT = 0xffff;
const ZIP64_FIELD = 0xffffffff;

// The members that count: files named .rwd. macOS adds a second file
// beside each one it zips, named "._" and the file's name, under
// __MACOSX/; a directory's name ends in a slash.
const EXPORT_SUFFIX = ".rwd";
const APPLEDOUBLE_PREFIX = "._";
const DIRECTORY_SUFFIX = "/";

// The largest export unpacked. The app holds an export in memory more
// than once while it reads it, in a browser tab's 4 GiB.
const MAX_EXPORT_BYTES = 512 * 1024 * 1024;
const MAX_EXPORT_WORDS = "512 MiB";

// Why a zip is not opened. Each reads after "is" or "answered", so the
// page and the app can both say the reason in a sentence.
export const Refusal = {
  Damaged: "a damaged zip",
  Zip64: "a zip in the zip64 format, which the viewer does not read",
  NoExport: "a zip with no .rwd file in it",
  ManyExports: "a zip with more than one .rwd file in it",
  TooLarge: `a zip whose .rwd file is larger than ${MAX_EXPORT_WORDS}`,
  Encrypted: "a zip with a password",
  Method: "a zip packed in a way the viewer does not read",
  NoInflater: "a zip this browser cannot unpack",
};

// A zip the viewer does not open, with the reason, one of Refusal's.
export class ZipRefusal extends Error {
  constructor(reason) {
    super(reason);
    this.reason = reason;
  }
}

// Whether `bytes` start as a zip does: with a member's local header, or
// with the end record of a zip that holds nothing.
export function isZip(bytes) {
  if (bytes.length < Uint32Array.BYTES_PER_ELEMENT) {
    return false;
  }
  const signature = viewOf(bytes).getUint32(0, true);
  return signature === LOCAL_SIGNATURE || signature === END_SIGNATURE;
}

// The one .rwd file in the zip `bytes`, as its name without directories
// and its bytes. Throws a ZipRefusal for a zip with none or several, and
// for one that cannot be read.
export async function rwdFromZip(bytes) {
  const view = viewOf(bytes);
  const exports = entriesOf(bytes, view).filter(isExport);
  if (exports.length === 0) {
    throw new ZipRefusal(Refusal.NoExport);
  }
  if (exports.length > 1) {
    throw new ZipRefusal(Refusal.ManyExports);
  }
  const [entry] = exports;

  // What cannot be unpacked is refused before any of it is.
  if (entry.flags & FLAG_ENCRYPTED) {
    throw new ZipRefusal(Refusal.Encrypted);
  }
  if (entry.size === ZIP64_FIELD || entry.packedSize === ZIP64_FIELD) {
    throw new ZipRefusal(Refusal.Zip64);
  }
  if (entry.size > MAX_EXPORT_BYTES) {
    throw new ZipRefusal(Refusal.TooLarge);
  }

  const packed = packedBytes(bytes, view, entry);
  const name = entry.name.slice(entry.name.lastIndexOf("/") + 1);
  if (entry.method === METHOD_STORED) {
    return { name, bytes: packed };
  }
  if (entry.method === METHOD_DEFLATE) {
    return { name, bytes: await inflate(packed, entry.size) };
  }
  throw new ZipRefusal(Refusal.Method);
}

// A view for reading the records' numbers out of `bytes`.
function viewOf(bytes) {
  return new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
}

// Whether a member is an export: a file named .rwd that macOS did not add.
function isExport(entry) {
  const name = entry.name.toLowerCase();
  const base = name.slice(name.lastIndexOf("/") + 1);
  return (
    name.endsWith(EXPORT_SUFFIX) &&
    !name.endsWith(DIRECTORY_SUFFIX) &&
    !base.startsWith(APPLEDOUBLE_PREFIX)
  );
}

// Every member the central directory lists.
function entriesOf(bytes, view) {
  const end = endRecord(bytes, view);
  const count = view.getUint16(end + END_ENTRIES, true);
  const directory = view.getUint32(end + END_DIRECTORY_OFFSET, true);
  if (count === ZIP64_COUNT || directory === ZIP64_FIELD) {
    throw new ZipRefusal(Refusal.Zip64);
  }

  const decoder = new TextDecoder();
  const entries = [];
  let at = directory;
  for (let i = 0; i < count; i++) {
    const inside = at + CENTRAL_SIZE <= bytes.length;
    if (!inside || view.getUint32(at, true) !== CENTRAL_SIGNATURE) {
      throw new ZipRefusal(Refusal.Damaged);
    }
    const nameLength = view.getUint16(at + CENTRAL_NAME_LENGTH, true);
    const extraLength = view.getUint16(at + CENTRAL_EXTRA_LENGTH, true);
    const commentLength = view.getUint16(at + CENTRAL_COMMENT_LENGTH, true);
    const name = at + CENTRAL_SIZE;
    if (name + nameLength > bytes.length) {
      throw new ZipRefusal(Refusal.Damaged);
    }
    entries.push({
      name: decoder.decode(bytes.subarray(name, name + nameLength)),
      flags: view.getUint16(at + CENTRAL_FLAGS, true),
      method: view.getUint16(at + CENTRAL_METHOD, true),
      packedSize: view.getUint32(at + CENTRAL_PACKED_SIZE, true),
      size: view.getUint32(at + CENTRAL_SIZE_FIELD, true),
      local: view.getUint32(at + CENTRAL_LOCAL_OFFSET, true),
    });
    at = name + nameLength + extraLength + commentLength;
  }
  return entries;
}

// Where the end record starts. It is the last record, followed only by a
// comment of a length the format bounds, so the search runs back from the
// end of the bytes over that many.
function endRecord(bytes, view) {
  const last = bytes.length - END_SIZE;
  const first = Math.max(0, last - END_COMMENT_MAX);
  for (let at = last; at >= first; at--) {
    if (view.getUint32(at, true) === END_SIGNATURE) {
      return at;
    }
  }
  throw new ZipRefusal(Refusal.Damaged);
}

// A member's bytes as the zip holds them, which follow its local header.
function packedBytes(bytes, view, entry) {
  const header = entry.local;
  const inside = header + LOCAL_SIZE <= bytes.length;
  if (!inside || view.getUint32(header, true) !== LOCAL_SIGNATURE) {
    throw new ZipRefusal(Refusal.Damaged);
  }
  const nameLength = view.getUint16(header + LOCAL_NAME_LENGTH, true);
  const extraLength = view.getUint16(header + LOCAL_EXTRA_LENGTH, true);
  const start = header + LOCAL_SIZE + nameLength + extraLength;
  const end = start + entry.packedSize;
  if (end > bytes.length) {
    throw new ZipRefusal(Refusal.Damaged);
  }
  return bytes.subarray(start, end);
}

// Inflates `packed` into the `size` bytes the directory says it holds.
// The output is never longer than that, whatever the packed bytes say, so
// a zip made to unpack without end is refused once it passes its own size.
async function inflate(packed, size) {
  if (typeof DecompressionStream === "undefined") {
    throw new ZipRefusal(Refusal.NoInflater);
  }
  const out = new Uint8Array(size);
  let filled = 0;
  try {
    const inflated = new Blob([packed])
      .stream()
      .pipeThrough(new DecompressionStream(DEFLATE_RAW));
    const reader = inflated.getReader();
    for (;;) {
      const { done, value } = await reader.read();
      if (done) {
        break;
      }
      if (filled + value.length > size) {
        await reader.cancel();
        throw new ZipRefusal(Refusal.Damaged);
      }
      out.set(value, filled);
      filled += value.length;
    }
  } catch (error) {
    // The inflater's own error means bytes that are not deflate's.
    if (error instanceof ZipRefusal) {
      throw error;
    }
    throw new ZipRefusal(Refusal.Damaged);
  }
  if (filled !== size) {
    throw new ZipRefusal(Refusal.Damaged);
  }
  return out;
}
