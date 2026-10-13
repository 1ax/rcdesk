//! STORED (uncompressed) ZIP archives of a folder, built on the fly for the
//! host -> client folder download (slice 3.4d). The archive size is known
//! exactly before the first byte is produced (`Manifest::zip_size`), so the
//! client can show progress and the transfer needs no temp file.
//!
//! Layout: per file a local header (general-purpose bit 3, real sizes, CRC 0),
//! the data, a 16-byte data descriptor with the CRC computed on the way; per
//! directory a bare local header; then the central directory and the EOCD.
//! No ZIP64: the limits below keep every field within 32/16 bits.
//!
//! Platform-independent: only `std` (plus tokio for async file reads).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::io::AsyncReadExt;

use super::CHUNK_BYTES;

/// Largest archive offered (same as the client's download limit).
pub const MAX_ZIP_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Largest entry count (the 16-bit field of the EOCD).
pub const MAX_ENTRIES: usize = 65535;

const LOCAL_SIG: u32 = 0x0403_4b50;
const DESCRIPTOR_SIG: u32 = 0x0807_4b50;
const CENTRAL_SIG: u32 = 0x0201_4b50;
const EOCD_SIG: u32 = 0x0605_4b50;
const FLAG_DESCRIPTOR: u16 = 1 << 3;
const FLAG_UTF8: u16 = 1 << 11;
const VERSION_NEEDED: u16 = 20;
const VERSION_MADE_BY: u16 = (3 << 8) | 20;
/// DOS 1980-01-01 00:00.
const DOS_EPOCH_DATE: u16 = (1 << 5) | 1;

// ---- CRC32 (IEEE) ----

const fn crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
}

static CRC_TABLE: [u32; 256] = crc_table();

/// Incremental CRC32 (polynomial 0xEDB88320).
#[derive(Clone, Copy)]
pub struct Crc32(u32);

impl Crc32 {
    pub fn new() -> Self {
        Self(0xFFFF_FFFF)
    }

    pub fn update(&mut self, data: &[u8]) {
        let mut c = self.0;
        for &b in data {
            c = CRC_TABLE[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
        }
        self.0 = c;
    }

    pub fn finish(self) -> u32 {
        self.0 ^ 0xFFFF_FFFF
    }
}

// ---- manifest ----

#[derive(Debug, Clone, PartialEq, Eq)]
enum Kind {
    Dir,
    File { size: u64 },
}

#[derive(Debug, Clone)]
struct Entry {
    /// Name inside the archive; directories end with `/`.
    name: String,
    path: PathBuf,
    kind: Kind,
    dos_time: u16,
    dos_date: u16,
}

impl Entry {
    fn data_len(&self) -> u64 {
        match self.kind {
            Kind::Dir => 0,
            Kind::File { size } => size,
        }
    }

    /// Bytes this entry adds to the archive (local part + central record).
    fn archive_bytes(&self) -> u64 {
        let name = self.name.len() as u64;
        let local = match self.kind {
            Kind::Dir => 30 + name,
            Kind::File { size } => 30 + name + size + 16,
        };
        local + 46 + name
    }
}

/// The files and directories of one folder, as seen at walk time.
#[derive(Debug)]
pub struct Manifest {
    entries: Vec<Entry>,
    limit: u64,
    overflow: bool,
    /// Exact archive size while not overflowed.
    size: u64,
}

impl Manifest {
    /// Exact size of the archive; `limit + 1` when the folder is too big.
    pub fn zip_size(&self) -> u64 {
        if self.overflow {
            self.limit + 1
        } else {
            self.size
        }
    }

    #[cfg(test)]
    fn is_overflow(&self) -> bool {
        self.overflow
    }

    #[cfg(test)]
    fn entry_count(&self) -> usize {
        self.entries.len()
    }
}

/// Walks `root` with the production limits.
pub fn walk(root: &Path) -> io::Result<Manifest> {
    walk_with_limit(root, MAX_ZIP_BYTES)
}

/// Walks `root`; stops once the archive would exceed `limit` bytes or
/// `MAX_ENTRIES` entries. Only an unreadable root is an error.
pub fn walk_with_limit(root: &Path, limit: u64) -> io::Result<Manifest> {
    let meta = fs::metadata(root)?;
    if !meta.is_dir() {
        return Err(io::Error::other("not a directory"));
    }
    let root_name = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "folder".to_string());
    // End-of-central-directory record.
    let mut m = Manifest {
        entries: Vec::new(),
        limit,
        overflow: false,
        size: 22,
    };
    let prefix = format!("{root_name}/");
    let (dos_time, dos_date) = dos_datetime(meta.modified().ok());
    if m.add(Entry {
        name: prefix.clone(),
        path: root.to_path_buf(),
        kind: Kind::Dir,
        dos_time,
        dos_date,
    }) {
        // A root that cannot be listed fails the whole walk.
        let children = sorted_children(root)?;
        walk_children(children, &prefix, &mut m);
    }
    Ok(m)
}

fn sorted_children(dir: &Path) -> io::Result<Vec<fs::DirEntry>> {
    let mut v = fs::read_dir(dir)?
        .filter_map(Result::ok)
        .collect::<Vec<_>>();
    v.sort_by_key(|e| e.file_name());
    Ok(v)
}

/// Returns `false` once the manifest overflowed (stop everything).
fn walk_children(children: Vec<fs::DirEntry>, prefix: &str, m: &mut Manifest) -> bool {
    for child in children {
        let path = child.path();
        // `symlink_metadata`: links are never followed (and never archived).
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        let file_name = child.file_name().to_string_lossy().into_owned();
        let (dos_time, dos_date) = dos_datetime(meta.modified().ok());
        if meta.is_dir() {
            let name = format!("{prefix}{file_name}/");
            if !m.add(Entry {
                name: name.clone(),
                path: path.clone(),
                kind: Kind::Dir,
                dos_time,
                dos_date,
            }) {
                return false;
            }
            match sorted_children(&path) {
                Ok(sub) => {
                    if !walk_children(sub, &name, m) {
                        return false;
                    }
                }
                Err(err) => {
                    tracing::debug!(path = %path.display(), ?err, "cannot list subfolder, skipping")
                }
            }
        } else if meta.is_file() {
            if file_name == ".DS_Store" {
                continue;
            }
            if !m.add(Entry {
                name: format!("{prefix}{file_name}"),
                path,
                kind: Kind::File { size: meta.len() },
                dos_time,
                dos_date,
            }) {
                return false;
            }
        }
        // Anything else (socket, fifo, ...) is skipped.
    }
    true
}

impl Manifest {
    /// Adds `entry`, or marks the manifest overflowed and returns `false`.
    fn add(&mut self, entry: Entry) -> bool {
        if entry.name.len() > u16::MAX as usize {
            tracing::debug!("entry name too long for zip, skipping");
            return true;
        }
        let new_size = self.size + entry.archive_bytes();
        if self.entries.len() + 1 > MAX_ENTRIES || new_size > self.limit {
            self.overflow = true;
            return false;
        }
        self.size = new_size;
        self.entries.push(entry);
        true
    }
}

// ---- DOS time ----

/// DOS (time, date) of `t` in UTC; 1980-01-01 00:00 when unknown or out of
/// the representable range (1980..=2107).
fn dos_datetime(t: Option<SystemTime>) -> (u16, u16) {
    let Some(secs) = t
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
    else {
        return (0, DOS_EPOCH_DATE);
    };
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (y, mo, d) = civil_from_days(days);
    if !(1980..=2107).contains(&y) {
        return (0, DOS_EPOCH_DATE);
    }
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let time = ((h as u16) << 11) | ((mi as u16) << 5) | (s as u16 / 2);
    let date = (((y - 1980) as u16) << 9) | ((mo as u16) << 5) | d as u16;
    (time, date)
}

/// Days since 1970-01-01 -> (year, month, day), proleptic Gregorian.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

// ---- streaming ----

enum State {
    /// Next entry to emit.
    Entries,
    /// Reading the data of the current file entry.
    Data {
        file: tokio::fs::File,
        remaining: u64,
        crc: Crc32,
    },
    /// Central directory and EOCD are queued; drain and finish.
    Done,
}

/// Produces the archive in chunks of at most `CHUNK_BYTES`.
pub struct ZipStream {
    entries: Vec<Entry>,
    next: usize,
    state: State,
    /// Bytes generated so far (headers + data).
    gen_pos: u64,
    local_offsets: Vec<u32>,
    crcs: Vec<u32>,
    /// Generated small parts not yet handed out; `head[head_pos..]` is live.
    head: Vec<u8>,
    head_pos: usize,
}

impl ZipStream {
    /// `manifest` must not be overflowed.
    pub fn new(manifest: Manifest) -> io::Result<Self> {
        if manifest.overflow {
            return Err(io::Error::other("folder too large for zip"));
        }
        let n = manifest.entries.len();
        Ok(Self {
            entries: manifest.entries,
            next: 0,
            state: State::Entries,
            gen_pos: 0,
            local_offsets: Vec::with_capacity(n),
            crcs: Vec::with_capacity(n),
            head: Vec::new(),
            head_pos: 0,
        })
    }

    /// Next piece of the archive, `None` once it is complete. An error (file
    /// unreadable or shorter than at walk time) leaves the archive broken.
    pub async fn next_chunk(&mut self) -> io::Result<Option<Vec<u8>>> {
        let mut out: Vec<u8> = Vec::with_capacity(CHUNK_BYTES);
        while out.len() < CHUNK_BYTES {
            if self.head_pos < self.head.len() {
                let n = (CHUNK_BYTES - out.len()).min(self.head.len() - self.head_pos);
                out.extend_from_slice(&self.head[self.head_pos..self.head_pos + n]);
                self.head_pos += n;
                if self.head_pos == self.head.len() {
                    self.head.clear();
                    self.head_pos = 0;
                }
                continue;
            }
            match &mut self.state {
                State::Done => break,
                State::Data {
                    file,
                    remaining,
                    crc,
                } => {
                    let want = (*remaining).min((CHUNK_BYTES - out.len()) as u64) as usize;
                    let start = out.len();
                    out.resize(start + want, 0);
                    file.read_exact(&mut out[start..]).await?;
                    crc.update(&out[start..]);
                    *remaining -= want as u64;
                    self.gen_pos += want as u64;
                    if *remaining == 0 {
                        let crc = crc.finish();
                        let size = self.entries[self.next - 1].data_len() as u32;
                        self.crcs.push(crc);
                        put_u32(&mut self.head, DESCRIPTOR_SIG);
                        put_u32(&mut self.head, crc);
                        put_u32(&mut self.head, size);
                        put_u32(&mut self.head, size);
                        self.gen_pos += 16;
                        self.state = State::Entries;
                    }
                }
                State::Entries => {
                    if self.next == self.entries.len() {
                        self.queue_central_directory();
                        self.state = State::Done;
                        continue;
                    }
                    let idx = self.next;
                    self.next += 1;
                    self.begin_entry(idx).await?;
                }
            }
        }
        Ok((!out.is_empty()).then_some(out))
    }

    /// Queues the local header of entry `idx` and opens its file.
    async fn begin_entry(&mut self, idx: usize) -> io::Result<()> {
        let e = &self.entries[idx];
        let size = e.data_len();
        let is_file = matches!(e.kind, Kind::File { .. });
        // Open first: a file that vanished aborts before any header is out.
        let file = if size > 0 {
            let f = tokio::fs::File::open(&e.path).await?;
            if !f.metadata().await?.is_file() {
                return Err(io::Error::other("not a regular file"));
            }
            Some(f)
        } else {
            None
        };
        let e = &self.entries[idx];
        self.local_offsets.push(self.gen_pos as u32);
        let start = self.head.len();
        put_u32(&mut self.head, LOCAL_SIG);
        put_u16(&mut self.head, VERSION_NEEDED);
        put_u16(
            &mut self.head,
            FLAG_UTF8 | if is_file { FLAG_DESCRIPTOR } else { 0 },
        );
        put_u16(&mut self.head, 0); // method: stored
        put_u16(&mut self.head, e.dos_time);
        put_u16(&mut self.head, e.dos_date);
        put_u32(&mut self.head, 0); // crc: in the descriptor
        put_u32(&mut self.head, size as u32);
        put_u32(&mut self.head, size as u32);
        put_u16(&mut self.head, e.name.len() as u16);
        put_u16(&mut self.head, 0); // extra
        self.head.extend_from_slice(e.name.as_bytes());
        self.gen_pos += (self.head.len() - start) as u64;
        match file {
            Some(file) => {
                self.state = State::Data {
                    file,
                    remaining: size,
                    crc: Crc32::new(),
                }
            }
            None if is_file => {
                // Empty file: descriptor right away.
                self.crcs.push(0);
                put_u32(&mut self.head, DESCRIPTOR_SIG);
                put_u32(&mut self.head, 0);
                put_u32(&mut self.head, 0);
                put_u32(&mut self.head, 0);
                self.gen_pos += 16;
            }
            None => self.crcs.push(0),
        }
        Ok(())
    }

    fn queue_central_directory(&mut self) {
        let cd_offset = self.gen_pos as u32;
        let start = self.head.len();
        for (i, e) in self.entries.iter().enumerate() {
            let is_file = matches!(e.kind, Kind::File { .. });
            let size = e.data_len() as u32;
            put_u32(&mut self.head, CENTRAL_SIG);
            put_u16(&mut self.head, VERSION_MADE_BY);
            put_u16(&mut self.head, VERSION_NEEDED);
            put_u16(
                &mut self.head,
                FLAG_UTF8 | if is_file { FLAG_DESCRIPTOR } else { 0 },
            );
            put_u16(&mut self.head, 0);
            put_u16(&mut self.head, e.dos_time);
            put_u16(&mut self.head, e.dos_date);
            put_u32(&mut self.head, self.crcs[i]);
            put_u32(&mut self.head, size);
            put_u32(&mut self.head, size);
            put_u16(&mut self.head, e.name.len() as u16);
            put_u16(&mut self.head, 0); // extra
            put_u16(&mut self.head, 0); // comment
            put_u16(&mut self.head, 0); // disk
            put_u16(&mut self.head, 0); // internal attrs
            let attrs: u32 = if is_file {
                0o100644 << 16
            } else {
                (0o040755 << 16) | 0x10
            };
            put_u32(&mut self.head, attrs);
            put_u32(&mut self.head, self.local_offsets[i]);
            self.head.extend_from_slice(e.name.as_bytes());
        }
        let cd_size = (self.head.len() - start) as u32;
        put_u32(&mut self.head, EOCD_SIG);
        put_u16(&mut self.head, 0);
        put_u16(&mut self.head, 0);
        put_u16(&mut self.head, self.entries.len() as u16);
        put_u16(&mut self.head, self.entries.len() as u16);
        put_u32(&mut self.head, cd_size);
        put_u32(&mut self.head, cd_offset);
        put_u16(&mut self.head, 0); // comment
        self.gen_pos += (self.head.len() - start) as u64;
    }
}

fn put_u16(v: &mut Vec<u8>, x: u16) {
    v.extend_from_slice(&x.to_le_bytes());
}

fn put_u32(v: &mut Vec<u8>, x: u32) {
    v.extend_from_slice(&x.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    pub(super) fn scratch() -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "rcdesk-zip-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// root/{a.txt, empty.bin, big.bin (> CHUNK), привет.txt, sub/{inner.txt, deep/}, void/}
    fn sample(base: &Path) -> PathBuf {
        let root = base.join("root");
        fs::create_dir_all(root.join("sub/deep")).unwrap();
        fs::create_dir_all(root.join("void")).unwrap();
        fs::write(root.join("a.txt"), b"hello").unwrap();
        fs::write(root.join("empty.bin"), b"").unwrap();
        let big: Vec<u8> = (0..CHUNK_BYTES * 2 + 123)
            .map(|i| (i % 251) as u8)
            .collect();
        fs::write(root.join("big.bin"), big).unwrap();
        fs::write(root.join("привет.txt"), "мир").unwrap();
        fs::write(root.join("sub/inner.txt"), b"inner").unwrap();
        root
    }

    async fn collect(m: Manifest) -> io::Result<(Vec<u8>, usize)> {
        let mut s = ZipStream::new(m)?;
        let mut all = Vec::new();
        let mut chunks = 0;
        while let Some(c) = s.next_chunk().await? {
            assert!(c.len() <= CHUNK_BYTES);
            all.extend_from_slice(&c);
            chunks += 1;
        }
        Ok((all, chunks))
    }

    fn u16_at(b: &[u8], i: usize) -> u16 {
        u16::from_le_bytes([b[i], b[i + 1]])
    }

    fn u32_at(b: &[u8], i: usize) -> u32 {
        u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
    }

    #[test]
    fn crc32_reference_value() {
        let mut c = Crc32::new();
        c.update(b"123456789");
        assert_eq!(c.finish(), 0xCBF4_3926);
        let mut c = Crc32::new();
        c.update(b"1234");
        c.update(b"56789");
        assert_eq!(c.finish(), 0xCBF4_3926);
    }

    #[test]
    fn dos_time_known_and_fallback() {
        // 2021-03-04 05:06:08 UTC = 1614834368
        let t = UNIX_EPOCH + std::time::Duration::from_secs(1_614_834_368);
        let (time, date) = dos_datetime(Some(t));
        assert_eq!(date, ((41u16) << 9) | (3 << 5) | 4);
        assert_eq!(time, (5 << 11) | (6 << 5) | 4);
        assert_eq!(dos_datetime(None), (0, DOS_EPOCH_DATE));
        assert_eq!(dos_datetime(Some(UNIX_EPOCH)), (0, DOS_EPOCH_DATE));
    }

    #[tokio::test]
    async fn stream_length_equals_zip_size() {
        let base = scratch();
        let m = walk(&sample(&base)).unwrap();
        let expected = m.zip_size();
        let (bytes, chunks) = collect(m).await.unwrap();
        assert_eq!(bytes.len() as u64, expected);
        assert!(chunks >= 3);
        let _ = fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn structure_signature_and_eocd() {
        let base = scratch();
        let m = walk(&sample(&base)).unwrap();
        // root, sub, sub/deep, void = 4 dirs; 5 files.
        let count = m.entry_count();
        assert_eq!(count, 9);
        let (b, _) = collect(m).await.unwrap();
        assert_eq!(&b[..4], b"PK\x03\x04");
        let eocd = b.len() - 22;
        assert_eq!(u32_at(&b, eocd), EOCD_SIG);
        assert_eq!(u16_at(&b, eocd + 8), 9);
        assert_eq!(u16_at(&b, eocd + 10), 9);
        let cd_size = u32_at(&b, eocd + 12) as usize;
        let cd_off = u32_at(&b, eocd + 16) as usize;
        assert_eq!(cd_off + cd_size, eocd);
        assert_eq!(u32_at(&b, cd_off), CENTRAL_SIG);
        let _ = fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn system_unzip_accepts_the_archive() {
        use std::process::Command;
        let base = scratch();
        let m = walk(&sample(&base)).unwrap();
        let (bytes, _) = collect(m).await.unwrap();
        let zip = base.join("out.zip");
        fs::write(&zip, bytes).unwrap();
        let test = Command::new("unzip")
            .arg("-t")
            .arg(&zip)
            .output()
            .expect("unzip must be in PATH");
        assert!(
            test.status.success(),
            "{}",
            String::from_utf8_lossy(&test.stdout)
        );
        // `unzip -l` mangles non-ASCII names under some locales (Apple's
        // build does it even for archives it wrote itself), so check the
        // names by extracting instead.
        let out = base.join("extracted");
        let extract = Command::new("unzip")
            .arg("-q")
            .arg(&zip)
            .arg("-d")
            .arg(&out)
            .output()
            .expect("unzip must be in PATH");
        assert!(extract.status.success());
        assert_eq!(
            fs::read(out.join("root/привет.txt")).unwrap(),
            "мир".as_bytes()
        );
        assert!(out.join("root/sub/deep").is_dir());
        assert!(out.join("root/void").is_dir());
        assert_eq!(fs::read(out.join("root/sub/inner.txt")).unwrap(), b"inner");
        let _ = fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_and_ds_store_are_skipped() {
        let base = scratch();
        let root = base.join("root");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("real.txt"), b"x").unwrap();
        fs::write(root.join(".DS_Store"), b"junk").unwrap();
        std::os::unix::fs::symlink(root.join("real.txt"), root.join("link.txt")).unwrap();
        std::os::unix::fs::symlink(&root, root.join("loop")).unwrap();
        let m = walk(&root).unwrap();
        let names: Vec<&str> = m.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["root/", "root/real.txt"]);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn overflow_marks_manifest_and_reports_limit_plus_one() {
        let base = scratch();
        let root = sample(&base);
        let m = walk_with_limit(&root, 1000).unwrap();
        assert!(m.is_overflow());
        assert_eq!(m.zip_size(), 1001);
        assert!(ZipStream::new(m).is_err());
        let ok = walk_with_limit(&root, MAX_ZIP_BYTES).unwrap();
        assert!(!ok.is_overflow());
        let _ = fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn file_shrunk_after_walk_is_an_error() {
        let base = scratch();
        let root = sample(&base);
        let m = walk(&root).unwrap();
        fs::write(root.join("big.bin"), b"short").unwrap();
        let err = collect(m).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
        let _ = fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn file_grown_after_walk_is_read_as_snapshot() {
        let base = scratch();
        let root = sample(&base);
        let m = walk(&root).unwrap();
        let expected = m.zip_size();
        let mut f = fs::OpenOptions::new()
            .append(true)
            .open(root.join("a.txt"))
            .unwrap();
        std::io::Write::write_all(&mut f, b" and more").unwrap();
        let (bytes, _) = collect(m).await.unwrap();
        assert_eq!(bytes.len() as u64, expected);
        let _ = fs::remove_dir_all(&base);
    }
}
