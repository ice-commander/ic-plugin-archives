pub mod pack;
use chrono::TimeZone;
use ic_plugin_api::{
    IcBytes, IcDirEntry, IcFsHandle, IcFsSource, IcFsVTable, IcHost, IcListing, IC_OPEN_READ,
    IC_OPEN_WRITE, IC_SEEK_END, IC_SEEK_SET,
};
use std::cell::RefCell;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::sync::atomic::{AtomicUsize, Ordering};

static HOST: AtomicUsize = AtomicUsize::new(0);

fn host() -> *const IcHost {
    HOST.load(Ordering::Relaxed) as *const IcHost
}

fn read_whole(source: IcFsSource, path: &CStr) -> Option<Vec<u8>> {
    let host = host();
    if host.is_null() {
        return None;
    }
    let stream = unsafe { ((*host).fs_open)(source, path.as_ptr(), IC_OPEN_READ) };
    if stream.is_null() {
        return None;
    }
    let end = unsafe { ((*host).fs_seek)(stream, 0, IC_SEEK_END) };
    unsafe { ((*host).fs_seek)(stream, 0, IC_SEEK_SET) };
    let mut held = Vec::with_capacity(end.max(0) as usize);
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = unsafe { ((*host).fs_read)(stream, buffer.as_mut_ptr(), buffer.len() as u64) };
        if read <= 0 {
            break;
        }
        held.extend_from_slice(&buffer[..read as usize]);
    }
    unsafe { ((*host).fs_close)(stream) };
    Some(held)
}

fn write_whole(o: &mut Opened) -> bool {
    let host = host();
    if host.is_null() || o.source.is_null() {
        o.error = CString::new("there is no filesystem to write to").unwrap_or_default();
        return false;
    }
    let stream = unsafe { ((*host).fs_open)(o.source, o.path.as_ptr(), IC_OPEN_WRITE) };
    if stream.is_null() {
        o.error = CString::new("the file could not be opened for writing").unwrap_or_default();
        return false;
    }
    let mut written = 0usize;
    while written < o.data.len() {
        let sent = unsafe {
            ((*host).fs_write)(
                stream,
                o.data[written..].as_ptr(),
                (o.data.len() - written) as u64,
            )
        };
        if sent <= 0 {
            break;
        }
        written += sent as usize;
    }
    unsafe { ((*host).fs_truncate)(stream, o.data.len() as u64) };
    unsafe { ((*host).fs_close)(stream) };
    if written != o.data.len() {
        o.error = CString::new("the archive could not be written out whole").unwrap_or_default();
        return false;
    }
    unsafe { ((*host).fs_changed)(o.source, o.path.as_ptr()) };
    true
}

pub const EXTENSIONS: &str = ".zip,.tar,.tar.gz,.tgz,.tar.bz2,.tbz2,.tbz";

#[derive(Debug)]
pub struct Error(String);

impl Error {
    pub fn new(text: impl Into<String>) -> Self {
        Self(text.into())
    }

    pub fn from_io(e: impl std::fmt::Display) -> Self {
        Self(e.to_string())
    }

    pub fn text(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug)]
pub struct ArchiveEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub permissions: Option<u32>,
    pub modified: u64,
    /// `None` for every tar format: it has no per-entry compressed size.
    pub packed: Option<u64>,
}

pub fn is_tar_format_from_name(name: &str) -> bool {
    let n = name.to_lowercase();
    n.ends_with(".tar.gz")
        || n.ends_with(".tgz")
        || n.ends_with(".tar.bz2")
        || n.ends_with(".tbz2")
        || n.ends_with(".tbz")
        || n.ends_with(".tar")
}

pub fn is_gzip_data(data: &[u8]) -> bool {
    data.len() >= 2 && data[0] == 0x1f && data[1] == 0x8b
}

pub fn is_bzip2_data(data: &[u8]) -> bool {
    data.len() >= 4 && &data[..3] == b"BZh" && data[3].is_ascii_digit() && data[3] != b'0'
}

pub fn tar_reader_mem(data: &[u8]) -> Box<dyn std::io::Read + '_> {
    if is_gzip_data(data) {
        Box::new(flate2::read::GzDecoder::new(data))
    } else if is_bzip2_data(data) {
        Box::new(bzip2::read::MultiBzDecoder::new(data))
    } else {
        Box::new(data)
    }
}

pub fn enclosed(name: &str) -> Option<String> {
    if name.contains('\0') {
        return None;
    }
    let unified = name.replace('\\', "/");
    if unified.starts_with('/') {
        return None;
    }
    let mut kept = Vec::new();
    // Judged one name at a time: the host joins each listed name onto its own folder.
    for part in unified.split('/') {
        let mut parsed = std::path::Path::new(part).components();
        match (parsed.next(), parsed.next()) {
            (None, _) | (Some(std::path::Component::CurDir), None) => continue,
            (Some(std::path::Component::Normal(_)), None) if !climbs_on_windows(part) => {
                kept.push(part)
            }
            _ => return None,
        }
    }
    Some(kept.join("/"))
}

// Win32 strips trailing spaces from a name, so `.. ` climbs like `..`.
fn climbs_on_windows(part: &str) -> bool {
    cfg!(windows) && part.contains(' ') && part.trim_end_matches([' ', '.']).is_empty()
}

fn within(path: &str) -> Option<String> {
    enclosed(path.replace('\\', "/").trim_start_matches('/'))
}

#[derive(Default)]
pub struct Scanned {
    pub entries: Vec<ArchiveEntry>,
    /// Position of each listed entry among the archive's own, so a read takes the very one listed.
    pub at: Vec<usize>,
    /// Entries a rewrite would lose or alter: not enclosed, shadowed by a later entry, or not plain files and folders.
    pub hidden: usize,
    seen: std::collections::HashMap<String, usize>,
    carried: Vec<bool>,
}

impl Scanned {
    fn keep(&mut self, raw: &str, at: usize, carried: bool, mut entry: ArchiveEntry) {
        let Some(name) = enclosed(raw) else {
            self.hidden += 1;
            return;
        };
        if !carried || name.is_empty() && !entry.is_dir {
            self.hidden += 1;
        }
        if name.is_empty() {
            return;
        }
        entry.name = name.clone();
        match self.seen.entry(name) {
            std::collections::hash_map::Entry::Vacant(free) => {
                free.insert(self.entries.len());
                self.entries.push(entry);
                self.at.push(at);
                self.carried.push(carried);
            }
            std::collections::hash_map::Entry::Occupied(taken) => {
                let slot = *taken.get();
                if self.carried[slot] && !(self.entries[slot].is_dir && entry.is_dir) {
                    self.hidden += 1;
                }
                self.entries[slot] = entry;
                self.at[slot] = at;
                self.carried[slot] = carried;
            }
        }
    }

    pub fn find(&self, path: &str) -> Option<usize> {
        let wanted = within(path).filter(|wanted| !wanted.is_empty())?;
        self.seen.get(&wanted).copied()
    }
}

pub fn scan_archive_from_memory(
    data: &[u8],
    _archive_path: &str,
    is_tar: bool,
) -> Result<Scanned, Error> {
    if is_tar {
        scan_tar_memory(data)
    } else {
        scan_zip_memory(data)
    }
}

const SPECIAL_BITS: u32 = 0o7000;

pub fn scan_zip_memory(data: &[u8]) -> Result<Scanned, Error> {
    let reader = std::io::Cursor::new(data);
    let mut archive = zip::ZipArchive::new(reader).map_err(|e| Error::new(e.to_string()))?;
    let mut scanned = Scanned::default();
    for i in 0..archive.len() {
        let Ok(entry) = archive.by_index(i) else {
            scanned.hidden += 1;
            continue;
        };
        let is_dir = entry.is_dir() || entry.name().ends_with('/');
        let permissions = entry.unix_mode();
        let carried =
            permissions.is_none_or(|mode| mode & 0o170000 != 0o120000 && mode & SPECIAL_BITS == 0);

        let modified = {
            let zip_dt = entry.last_modified();
            let year = zip_dt.year() as i32;
            let month = zip_dt.month() as u32;
            let day = zip_dt.day() as u32;
            let hour = zip_dt.hour() as u32;
            let minute = zip_dt.minute() as u32;
            let second = zip_dt.second() as u32;

            chrono::NaiveDate::from_ymd_opt(year, month, day)
                .and_then(|d| d.and_hms_opt(hour, minute, second))
                .and_then(|ndt| chrono::Local.from_local_datetime(&ndt).single())
                .map(|dt| dt.timestamp() as u64)
                .unwrap_or(0)
        };

        let listed = ArchiveEntry {
            name: String::new(),
            is_dir,
            size: entry.size(),
            permissions,
            modified,
            packed: Some(entry.compressed_size()),
        };
        scanned.keep(entry.name(), i, carried, listed);
    }
    Ok(scanned)
}

pub fn scan_tar_memory(data: &[u8]) -> Result<Scanned, Error> {
    let mut scanned = Scanned::default();
    let mut archive = tar::Archive::new(tar_reader_mem(data));
    if let Ok(tar_entries) = archive.entries() {
        for (at, entry) in tar_entries.enumerate() {
            let Ok(entry) = entry else {
                scanned.hidden += 1;
                break;
            };
            let Ok(p) = entry.path() else {
                scanned.hidden += 1;
                continue;
            };
            let kind = entry.header().entry_type();
            let is_dir = kind.is_dir();
            let mode = entry.header().mode().ok();
            let carried = (kind.is_file() || is_dir || kind.is_gnu_sparse())
                && p.to_str().is_some()
                && mode.is_none_or(|mode| mode & SPECIAL_BITS == 0);
            let listed = ArchiveEntry {
                name: String::new(),
                is_dir,
                size: entry.size(),
                // tar's own unpack drops setuid, setgid and sticky the same way.
                permissions: mode.map(|mode| mode & 0o777),
                modified: entry.header().mtime().unwrap_or(0),
                packed: None,
            };
            scanned.keep(&p.to_string_lossy(), at, carried, listed);
        }
    }
    Ok(scanned)
}

pub fn read_listed(data: &[u8], is_tar: bool, at: usize) -> Result<Vec<u8>, Error> {
    let gone = || Error::new("the entry is no longer in the archive");
    if is_tar {
        let mut tar = tar::Archive::new(tar_reader_mem(data));
        let mut entry = tar
            .entries()
            .map_err(Error::from_io)?
            .nth(at)
            .ok_or_else(gone)?
            .map_err(Error::from_io)?;
        if entry.header().entry_type().is_dir() {
            return Err(Error::new("Is a directory"));
        }
        let mut content = Vec::with_capacity(entry.size() as usize);
        std::io::copy(&mut entry, &mut content).map_err(Error::from_io)?;
        Ok(content)
    } else {
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(data))
            .map_err(|e| Error::new(e.to_string()))?;
        if at >= archive.len() {
            return Err(gone());
        }
        let mut entry = archive
            .by_index(at)
            .map_err(|e| Error::new(e.to_string()))?;
        if entry.is_dir() || entry.name().ends_with('/') {
            return Err(Error::new("Is a directory"));
        }
        let mut content = Vec::with_capacity(entry.size() as usize);
        std::io::copy(&mut entry, &mut content).map_err(Error::from_io)?;
        Ok(content)
    }
}

pub fn read_file_from_archive_memory(
    data: &[u8],
    internal_file: &str,
    is_tar: bool,
) -> Result<Vec<u8>, Error> {
    let scanned = scan_archive_from_memory(data, "", is_tar)?;
    read_from(&scanned, data, internal_file, is_tar)
}

fn read_from(
    scanned: &Scanned,
    data: &[u8],
    internal_file: &str,
    is_tar: bool,
) -> Result<Vec<u8>, Error> {
    match scanned.find(internal_file) {
        Some(slot) => read_listed(data, is_tar, scanned.at[slot]),
        None => Err(Error::new(format!(
            "File not found in archive: {}",
            internal_file
        ))),
    }
}

pub fn normalize(path: &str) -> String {
    path.replace('\\', "/").trim_matches('/').to_string()
}

pub fn packed_cells(entry: &ArchiveEntry) -> (String, String) {
    let Some(packed) = entry.packed else {
        return (String::new(), String::new());
    };
    if entry.is_dir {
        return (String::new(), String::new());
    }
    let size = human(packed);
    if entry.size == 0 {
        return (size, String::new());
    }
    let ratio = (packed as f64 / entry.size as f64 * 100.0).round() as u64;
    (size, format!("{ratio}%"))
}

pub fn human(bytes: u64) -> String {
    const STEP: f64 = 1024.0;
    let names = ["B", "KB", "MB", "GB", "TB"];
    let mut left = bytes as f64;
    let mut at = 0;
    while left >= STEP && at + 1 < names.len() {
        left /= STEP;
        at += 1;
    }
    if at == 0 {
        format!("{bytes} {}", names[0])
    } else {
        format!("{left:.1} {}", names[at])
    }
}

pub fn level(entries: &[ArchiveEntry], internal_dir: &str) -> Vec<ArchiveEntry> {
    let dir = normalize(internal_dir);
    let prefix = if dir.is_empty() {
        String::new()
    } else {
        format!("{}/", dir)
    };
    let mut dirs: Vec<String> = Vec::new();
    let mut files: Vec<ArchiveEntry> = Vec::new();

    for entry in entries {
        let full = normalize(&entry.name);
        let rest = match full.strip_prefix(prefix.as_str()) {
            Some(r) if !prefix.is_empty() => r,
            _ if prefix.is_empty() => full.as_str(),
            _ => continue,
        };
        let mut parts = rest.splitn(2, '/');
        let head = match parts.next() {
            Some(h) if !h.is_empty() => h,
            _ => continue,
        };
        if parts.next().is_some() || entry.is_dir {
            if !dirs.iter().any(|d| d == head) {
                dirs.push(head.to_string());
            }
        } else if !files.iter().any(|f| f.name == head) {
            files.push(ArchiveEntry {
                name: head.to_string(),
                is_dir: false,
                size: entry.size,
                permissions: entry.permissions,
                modified: entry.modified,
                packed: entry.packed,
            });
        }
    }

    files.retain(|f| !dirs.iter().any(|d| *d == f.name));
    let mut out: Vec<ArchiveEntry> = dirs
        .into_iter()
        .map(|name| ArchiveEntry {
            name,
            is_dir: true,
            size: 0,
            permissions: Some(0o755),
            modified: 0,
            packed: None,
        })
        .collect();
    out.extend(files);
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

struct Opened {
    source: IcFsSource,
    path: CString,
    listed: Scanned,
    is_tar: bool,
    packing: Option<pack::Packing>,
    data: Vec<u8>,
    // Backing storage for pointers handed to the host; must outlive its read of the answer.
    names: Vec<CString>,
    view: Vec<IcDirEntry>,
    cells: Vec<CString>,
    cell_ptrs: Vec<*const c_char>,
    rows: Vec<ic_plugin_api::IcRow>,
    columns: Vec<ic_plugin_api::IcFsColumn>,
    bytes: Vec<u8>,
    error: CString,
}

extern "C" fn fs_open_in(
    source: IcFsSource,
    path: *const c_char,
    _user_data: *mut c_void,
) -> IcFsHandle {
    if path.is_null() {
        return std::ptr::null_mut();
    }
    let held = unsafe { CStr::from_ptr(path) }.to_owned();
    let file_name = held.to_string_lossy().to_string();
    let is_tar = is_tar_format_from_name(&file_name);
    let packing = pack::packing_for(&file_name);
    // An empty file is a new archive the host just created; it reaches the disk on the first write.
    let read = read_whole(source, &held).unwrap_or_default();
    let data = if read.is_empty() {
        match packing.and_then(|packing| pack::pack(&[], packing).ok()) {
            Some(empty) => empty,
            None => return std::ptr::null_mut(),
        }
    } else {
        read
    };
    let scanned = match scan_archive_from_memory(&data, &file_name, is_tar) {
        Ok(e) => e,
        Err(_) => return std::ptr::null_mut(),
    };
    let opened = Box::new(RefCell::new(Opened {
        source,
        path: held,
        listed: scanned,
        is_tar,
        packing,
        data,
        names: Vec::new(),
        view: Vec::new(),
        cells: Vec::new(),
        cell_ptrs: Vec::new(),
        rows: Vec::new(),
        columns: Vec::new(),
        bytes: Vec::new(),
        error: CString::default(),
    }));
    Box::into_raw(opened) as IcFsHandle
}

fn with_opened<R>(handle: IcFsHandle, f: impl FnOnce(&mut Opened) -> R) -> Option<R> {
    if handle.is_null() {
        return None;
    }
    let cell = unsafe { &*(handle as *const RefCell<Opened>) };
    let mut borrowed = cell.borrow_mut();
    Some(f(&mut borrowed))
}

extern "C" fn fs_close(handle: IcFsHandle) {
    if handle.is_null() {
        return;
    }
    drop(unsafe { Box::from_raw(handle as *mut RefCell<Opened>) });
}

extern "C" fn fs_list(handle: IcFsHandle, path: *const c_char) -> IcListing {
    let Some(wanted) = inside(path) else {
        return IcListing::EMPTY;
    };
    with_opened(handle, |o| {
        let rows = level(&o.listed.entries, &wanted);
        o.names = rows
            .iter()
            .map(|r| CString::new(r.name.clone()).unwrap_or_default())
            .collect();
        o.view = rows
            .iter()
            .enumerate()
            .map(|(i, r)| IcDirEntry {
                name: o.names[i].as_ptr(),
                is_dir: if r.is_dir { 1 } else { 0 },
                size: r.size,
                modified: r.modified,
                permissions: r.permissions.unwrap_or(0),
                has_permissions: if r.permissions.is_some() { 1 } else { 0 },
            })
            .collect();
        IcListing {
            items: o.view.as_ptr(),
            count: o.view.len() as u32,
        }
    })
    .unwrap_or(IcListing::EMPTY)
}

extern "C" fn fs_columns(handle: IcFsHandle) -> ic_plugin_api::IcColumns {
    with_opened(handle, |o| {
        o.columns = vec![
            ic_plugin_api::IcFsColumn {
                key: c"archive_packed".as_ptr(),
                title: c"archives.packed".as_ptr(),
                width: 90,
                kind: ic_plugin_api::IC_COLUMN_TEXT,
            },
            ic_plugin_api::IcFsColumn {
                key: c"archive_ratio".as_ptr(),
                title: c"archives.ratio".as_ptr(),
                width: 70,
                kind: ic_plugin_api::IC_COLUMN_TEXT,
            },
        ];
        ic_plugin_api::IcColumns {
            count: o.columns.len() as u32,
            items: o.columns.as_ptr(),
            ..ic_plugin_api::IcColumns::EMPTY
        }
    })
    .unwrap_or(ic_plugin_api::IcColumns::EMPTY)
}

extern "C" fn fs_list_rows(handle: IcFsHandle, path: *const c_char) -> ic_plugin_api::IcRows {
    let Some(wanted) = inside(path) else {
        return ic_plugin_api::IcRows::EMPTY;
    };
    with_opened(handle, |o| {
        let rows = level(&o.listed.entries, &wanted);
        o.names = rows
            .iter()
            .map(|r| CString::new(r.name.clone()).unwrap_or_default())
            .collect();
        o.view = rows
            .iter()
            .enumerate()
            .map(|(i, r)| IcDirEntry {
                name: o.names[i].as_ptr(),
                is_dir: if r.is_dir { 1 } else { 0 },
                size: r.size,
                modified: r.modified,
                permissions: r.permissions.unwrap_or(0),
                has_permissions: if r.permissions.is_some() { 1 } else { 0 },
            })
            .collect();

        o.cells = rows
            .iter()
            .flat_map(|r| {
                let (packed, ratio) = packed_cells(r);
                [
                    CString::new(packed).unwrap_or_default(),
                    CString::new(ratio).unwrap_or_default(),
                ]
            })
            .collect();
        o.cell_ptrs = o.cells.iter().map(|c| c.as_ptr()).collect();
        o.rows = o
            .view
            .iter()
            .enumerate()
            .map(|(at, entry)| ic_plugin_api::IcRow {
                entry: *entry,
                extra: unsafe { o.cell_ptrs.as_ptr().add(at * 2) },
                extra_count: 2,
            })
            .collect();
        ic_plugin_api::IcRows {
            count: o.rows.len() as u32,
            items: o.rows.as_ptr(),
            ..ic_plugin_api::IcRows::EMPTY
        }
    })
    .unwrap_or(ic_plugin_api::IcRows::EMPTY)
}

extern "C" fn fs_read(handle: IcFsHandle, path: *const c_char) -> IcBytes {
    let wanted = if path.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(path) }
            .to_string_lossy()
            .to_string()
    };
    with_opened(handle, |o| {
        match read_from(&o.listed, &o.data, &wanted, o.is_tar) {
            Ok(content) => {
                o.bytes = content;
                IcBytes {
                    data: o.bytes.as_ptr(),
                    len: o.bytes.len() as u64,
                }
            }
            Err(e) => {
                o.error = CString::new(e.text().to_string()).unwrap_or_default();
                IcBytes::EMPTY
            }
        }
    })
    .unwrap_or(IcBytes::EMPTY)
}

extern "C" fn fs_is_read_only(handle: IcFsHandle) -> c_int {
    match with_opened(handle, |o| o.packing.is_some()) {
        Some(true) => 0,
        _ => 1,
    }
}

fn inside(path: *const c_char) -> Option<String> {
    if path.is_null() {
        return Some(String::new());
    }
    within(&unsafe { CStr::from_ptr(path) }.to_string_lossy())
}

fn taken_apart(o: &Opened) -> Result<Vec<pack::Entry>, String> {
    let mut carried = Vec::with_capacity(o.listed.entries.len());
    for (entry, &at) in o.listed.entries.iter().zip(&o.listed.at) {
        let bytes = if entry.is_dir {
            Vec::new()
        } else {
            read_listed(&o.data, o.is_tar, at).map_err(|why| why.text().to_string())?
        };
        carried.push(pack::Entry {
            // Compared without the trailing slash the format wrote, so `docs` never doubles.
            path: entry.name.trim_end_matches('/').to_string(),
            is_dir: entry.is_dir,
            bytes,
            permissions: entry.permissions,
            modified: Some(entry.modified).filter(|&modified| modified != 0),
        });
    }
    Ok(carried)
}

fn put_together(o: &mut Opened, entries: &[pack::Entry]) -> bool {
    let Some(packing) = o.packing else {
        o.error = CString::new("this archive format can only be read").unwrap_or_default();
        return false;
    };
    let packed = match pack::pack(entries, packing) {
        Ok(packed) => packed,
        Err(why) => {
            o.error = CString::new(why).unwrap_or_default();
            return false;
        }
    };
    match scan_archive_from_memory(&packed, "", o.is_tar) {
        Ok(listed) => {
            o.listed = listed;
            o.data = packed;
            // Written back at once: nothing else does it, and a mount dropped unwritten loses the change.
            write_whole(o)
        }
        Err(why) => {
            o.error = CString::new(why.text()).unwrap_or_default();
            false
        }
    }
}

fn rewritten(handle: IcFsHandle, change: impl FnOnce(&mut Vec<pack::Entry>) -> bool) -> c_int {
    with_opened(handle, |o| {
        if o.listed.hidden > 0 {
            o.error = CString::new(format!(
                "the archive holds {} entries that a rewrite cannot carry over (paths outside the archive, repeated names, links, special modes or broken entries); rewriting it would lose them",
                o.listed.hidden
            ))
            .unwrap_or_default();
            return ic_plugin_api::IC_ERR_IO;
        }
        let mut entries = match taken_apart(o) {
            Ok(entries) => entries,
            Err(why) => {
                o.error = CString::new(why).unwrap_or_default();
                return ic_plugin_api::IC_ERR_IO;
            }
        };
        if !change(&mut entries) {
            o.error = CString::new("nothing to change there").unwrap_or_default();
            return ic_plugin_api::IC_ERR_IO;
        }
        if put_together(o, &entries) {
            ic_plugin_api::IC_OK
        } else {
            ic_plugin_api::IC_ERR_IO
        }
    })
    .unwrap_or(ic_plugin_api::IC_ERR_IO)
}

fn now() -> Option<u64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|since| since.as_secs())
}

extern "C" fn fs_write(
    handle: IcFsHandle,
    path: *const c_char,
    bytes: *const u8,
    len: u64,
) -> c_int {
    let Some(at) = inside(path).filter(|at| !at.is_empty()) else {
        return ic_plugin_api::IC_ERR_IO;
    };
    if bytes.is_null() {
        return ic_plugin_api::IC_ERR_IO;
    }
    let content = unsafe { std::slice::from_raw_parts(bytes, len as usize) }.to_vec();
    rewritten(handle, move |entries| {
        let permissions = entries
            .iter()
            .find(|held| held.path == at && !held.is_dir)
            .and_then(|held| held.permissions);
        entries.retain(|held| held.path != at);
        entries.push(pack::Entry {
            path: at,
            is_dir: false,
            bytes: content,
            permissions,
            modified: now(),
        });
        true
    })
}

extern "C" fn fs_create_dir(handle: IcFsHandle, path: *const c_char) -> c_int {
    let Some(at) = inside(path).filter(|at| !at.is_empty()) else {
        return ic_plugin_api::IC_ERR_IO;
    };
    rewritten(handle, move |entries| {
        if entries.iter().any(|held| held.path == at) {
            return false;
        }
        entries.push(pack::Entry {
            path: at,
            is_dir: true,
            bytes: Vec::new(),
            permissions: None,
            modified: now(),
        });
        true
    })
}

extern "C" fn fs_remove(handle: IcFsHandle, path: *const c_char) -> c_int {
    let Some(at) = inside(path).filter(|at| !at.is_empty()) else {
        return ic_plugin_api::IC_ERR_IO;
    };
    let under = format!("{}/", at.trim_end_matches('/'));
    rewritten(handle, move |entries| {
        let before = entries.len();
        entries.retain(|held| held.path != at && !held.path.starts_with(&under));
        entries.len() != before
    })
}

extern "C" fn fs_last_error(handle: IcFsHandle) -> *const c_char {
    with_opened(handle, |o| o.error.as_ptr()).unwrap_or(std::ptr::null())
}

pub fn vtable() -> IcFsVTable {
    IcFsVTable {
        struct_size: std::mem::size_of::<IcFsVTable>() as u32,
        open_in: fs_open_in,
        close: fs_close,
        list: fs_list,
        read: fs_read,
        is_read_only: fs_is_read_only,
        last_error: fs_last_error,
        write: Some(fs_write),
        create_dir: Some(fs_create_dir),
        remove: Some(fs_remove),
        rename: None,
        shell_open: None,
        shell_read: None,
        shell_write: None,
        shell_resize: None,
        shell_close: None,
        shell_available: None,
        columns: Some(fs_columns),
        list_rows: Some(fs_list_rows),
        action_state: None,
        cell_clicked: None,
        set_permissions: None,
    }
}

include!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../version.rs"));

ic_plugin_api::declare_about!(
    "ic-archives-fs",
    "Archives",
    plugins_version!(),
    "Opens zip and tar archives as folders"
);

#[cfg_attr(feature = "export-abi", no_mangle)]
pub extern "C" fn ic_plugin_init(host: *const IcHost, _kind: *const c_char) -> c_int {
    match ic_plugin_api::check_host(
        host,
        ic_plugin_api::IC_ABI_VERSION,
        std::mem::size_of::<IcHost>() as u32,
    ) {
        ic_plugin_api::HostCheck::Ok => {}
        ic_plugin_api::HostCheck::WrongMagic => return ic_plugin_api::IC_ERR_HOST_UNKNOWN,
        _ => return ic_plugin_api::IC_ERR_HOST_TOO_OLD,
    }
    HOST.store(host as usize, Ordering::Relaxed);
    for (language, catalogue) in LOCALES {
        let Ok(tag) = CString::new(*language) else {
            continue;
        };
        unsafe {
            ((*host).register_locales)(tag.as_ptr(), catalogue.as_ptr(), catalogue.len() as u64)
        };
    }
    let Ok(exts) = CString::new(EXTENSIONS) else {
        return ic_plugin_api::IC_ERR_INIT_FAILED;
    };
    // The host copies the table rather than keeping this pointer, so it may live on the stack.
    let table = vtable();
    unsafe { ((*host).register_filesystem)(exts.as_ptr(), &table, std::ptr::null_mut()) }
}

const LOCALES: &[(&str, &str)] = &[
    ("en", include_str!("../locales/en.json")),
    ("ru", include_str!("../locales/ru.json")),
    ("pl", include_str!("../locales/pl.json")),
    ("cs", include_str!("../locales/cs.json")),
    ("sk", include_str!("../locales/sk.json")),
    ("de", include_str!("../locales/de.json")),
    ("es", include_str!("../locales/es.json")),
    ("uk", include_str!("../locales/uk.json")),
    ("it", include_str!("../locales/it.json")),
    ("fr", include_str!("../locales/fr.json")),
    ("ro", include_str!("../locales/ro.json")),
    ("hu", include_str!("../locales/hu.json")),
    ("be", include_str!("../locales/be.json")),
    ("bg", include_str!("../locales/bg.json")),
    ("sr", include_str!("../locales/sr.json")),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_zip_entry_says_how_much_room_it_takes_and_a_tar_entry_does_not() {
        let entry = ArchiveEntry {
            name: "notes.txt".to_string(),
            is_dir: false,
            size: 1000,
            permissions: None,
            modified: 0,
            packed: Some(250),
        };
        assert_eq!(
            packed_cells(&entry),
            ("250 B".to_string(), "25%".to_string())
        );

        let in_a_tar = ArchiveEntry {
            packed: None,
            ..entry.clone()
        };
        assert_eq!(packed_cells(&in_a_tar), (String::new(), String::new()));
    }

    #[test]
    fn nothing_is_claimed_for_a_directory_or_a_file_of_nothing() {
        let directory = ArchiveEntry {
            name: "src".to_string(),
            is_dir: true,
            size: 0,
            permissions: None,
            modified: 0,
            packed: Some(0),
        };
        assert_eq!(packed_cells(&directory), (String::new(), String::new()));

        let empty = ArchiveEntry {
            is_dir: false,
            ..directory
        };
        assert_eq!(packed_cells(&empty), ("0 B".to_string(), String::new()));
    }

    #[test]
    fn a_file_that_did_not_compress_reads_as_a_hundred_per_cent() {
        let entry = ArchiveEntry {
            name: "already.jpg".to_string(),
            is_dir: false,
            size: 4096,
            permissions: None,
            modified: 0,
            packed: Some(4096),
        };
        assert_eq!(
            packed_cells(&entry),
            ("4.0 KB".to_string(), "100%".to_string())
        );
    }

    #[test]
    fn room_is_written_the_way_a_person_reads_it() {
        assert_eq!(human(0), "0 B");
        assert_eq!(human(999), "999 B");
        assert_eq!(human(1024), "1.0 KB");
        assert_eq!(human(1536), "1.5 KB");
        assert_eq!(human(1024 * 1024), "1.0 MB");
        assert_eq!(human(3 * 1024 * 1024 * 1024), "3.0 GB");
    }

    #[test]
    fn every_language_carries_every_heading() {
        let english: std::collections::BTreeSet<String> =
            serde_json::from_str::<std::collections::BTreeMap<String, String>>(LOCALES[0].1)
                .expect("a catalogue")
                .into_keys()
                .collect();
        assert_eq!(LOCALES.len(), 15);
        for (language, catalogue) in LOCALES {
            let table: std::collections::BTreeMap<String, String> = serde_json::from_str(catalogue)
                .unwrap_or_else(|_| panic!("`{language}` unreadable"));
            assert_eq!(
                table
                    .keys()
                    .cloned()
                    .collect::<std::collections::BTreeSet<_>>(),
                english,
                "`{language}` does not carry the same headings"
            );
            assert!(table.values().all(|word| !word.trim().is_empty()));
        }
    }
    use std::io::Write;

    fn sample_zip() -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let cursor = std::io::Cursor::new(&mut buf);
            let mut zip = zip::ZipWriter::new(cursor);
            let opts = zip::write::FileOptions::default();
            zip.start_file("readme.txt", opts).unwrap();
            zip.write_all(b"hello archive").unwrap();
            zip.add_directory("docs/", opts).unwrap();
            zip.start_file("docs/inner.txt", opts).unwrap();
            zip.write_all(b"inner file").unwrap();
            zip.finish().unwrap();
        }
        buf
    }

    mod as_if_hosted {
        use super::*;
        use std::io::{Read, Seek, SeekFrom, Write};

        struct Stream(std::fs::File);

        pub extern "C" fn fs_caps(_: IcFsSource) -> u32 {
            ic_plugin_api::IC_FS_SEEK | ic_plugin_api::IC_FS_WRITE | ic_plugin_api::IC_FS_LIST
        }

        pub extern "C" fn fs_open(
            source: IcFsSource,
            path: *const c_char,
            mode: u32,
        ) -> ic_plugin_api::IcStream {
            let root = unsafe { &*(source as *const std::path::PathBuf) };
            let at = root.join(unsafe { CStr::from_ptr(path) }.to_string_lossy().as_ref());
            let opened = if mode == IC_OPEN_WRITE {
                std::fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(at)
            } else {
                std::fs::File::open(at)
            };
            match opened {
                Ok(file) => Box::into_raw(Box::new(Stream(file))) as ic_plugin_api::IcStream,
                Err(_) => std::ptr::null_mut(),
            }
        }

        pub extern "C" fn fs_read(stream: ic_plugin_api::IcStream, into: *mut u8, len: u64) -> i64 {
            let held = unsafe { &mut *(stream as *mut Stream) };
            let room = unsafe { std::slice::from_raw_parts_mut(into, len as usize) };
            held.0.read(room).map(|read| read as i64).unwrap_or(-1)
        }

        pub extern "C" fn fs_seek(
            stream: ic_plugin_api::IcStream,
            offset: i64,
            whence: u32,
        ) -> i64 {
            let held = unsafe { &mut *(stream as *mut Stream) };
            let wanted = match whence {
                IC_SEEK_SET => SeekFrom::Start(offset.max(0) as u64),
                IC_SEEK_END => SeekFrom::End(offset),
                _ => SeekFrom::Current(offset),
            };
            held.0.seek(wanted).map(|at| at as i64).unwrap_or(-1)
        }

        pub extern "C" fn fs_write(
            stream: ic_plugin_api::IcStream,
            from: *const u8,
            len: u64,
        ) -> i64 {
            let held = unsafe { &mut *(stream as *mut Stream) };
            let bytes = unsafe { std::slice::from_raw_parts(from, len as usize) };
            held.0.write(bytes).map(|sent| sent as i64).unwrap_or(-1)
        }

        pub extern "C" fn fs_truncate(stream: ic_plugin_api::IcStream, len: u64) -> c_int {
            let held = unsafe { &mut *(stream as *mut Stream) };
            match held.0.set_len(len) {
                Ok(()) => ic_plugin_api::IC_OK,
                Err(_) => ic_plugin_api::IC_ERR_INIT_FAILED,
            }
        }

        pub extern "C" fn fs_close(stream: ic_plugin_api::IcStream) {
            if !stream.is_null() {
                drop(unsafe { Box::from_raw(stream as *mut Stream) });
            }
        }

        pub extern "C" fn fs_list(_: IcFsSource, _: *const c_char) -> IcListing {
            IcListing::EMPTY
        }

        pub extern "C" fn fs_local_path(_: IcFsSource, _: *const c_char) -> *const c_char {
            std::ptr::null()
        }

        pub extern "C" fn fs_changed(_: IcFsSource, _: *const c_char) -> c_int {
            ic_plugin_api::IC_OK
        }
    }

    struct Somewhere(std::path::PathBuf, IcFsSource);

    impl Drop for Somewhere {
        fn drop(&mut self) {
            drop(unsafe { Box::from_raw(self.1 as *mut std::path::PathBuf) });
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    impl Somewhere {
        fn holding(bytes: &[u8], name: &str) -> Somewhere {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let at = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let root =
                std::env::temp_dir().join(format!("ic-archives-test-{}-{at}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).expect("a directory of our own");
            std::fs::write(root.join(name), bytes).expect("the file the mount lives in");
            let source = Box::into_raw(Box::new(root.clone())) as IcFsSource;
            Somewhere(root, source)
        }

        fn as_it_stands(&self, name: &str) -> Vec<u8> {
            std::fs::read(self.0.join(name)).expect("the file the mount lives in")
        }
    }

    fn hosted() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            let mut table = ic_plugin_api::testing::silent_host();
            table.fs_caps = as_if_hosted::fs_caps;
            table.fs_open = as_if_hosted::fs_open;
            table.fs_read = as_if_hosted::fs_read;
            table.fs_seek = as_if_hosted::fs_seek;
            table.fs_write = as_if_hosted::fs_write;
            table.fs_truncate = as_if_hosted::fs_truncate;
            table.fs_close = as_if_hosted::fs_close;
            table.fs_list = as_if_hosted::fs_list;
            table.fs_local_path = as_if_hosted::fs_local_path;
            table.fs_changed = as_if_hosted::fs_changed;
            let table = Box::leak(Box::new(table));
            HOST.store(table as *const IcHost as usize, Ordering::Relaxed);
        });
    }

    fn open_in(held: &Somewhere, name: &str) -> IcFsHandle {
        hosted();
        let name = CString::new(name).unwrap();
        fs_open_in(held.1, name.as_ptr(), std::ptr::null_mut())
    }

    fn open(bytes: &[u8], name: &str) -> IcFsHandle {
        let held = Somewhere::holding(bytes, name);
        let handle = open_in(&held, name);
        // Leaked on purpose: the handle keeps using the source and its directory.
        std::mem::forget(held);
        handle
    }

    fn sample_tar_gz() -> Vec<u8> {
        let mut tar = Vec::new();
        {
            let mut writer = tar::Builder::new(&mut tar);
            let mut add = |path: &str, body: &[u8]| {
                let mut header = tar::Header::new_gnu();
                header.set_path(path).unwrap();
                header.set_size(body.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                writer.append(&header, body).unwrap();
            };
            add("notes.txt", b"tar contents");
            add("deep/one/two.txt", b"buried");
            writer.finish().unwrap();
        }
        let mut packed = Vec::new();
        {
            let mut gzip =
                flate2::write::GzEncoder::new(&mut packed, flate2::Compression::default());
            gzip.write_all(&tar).unwrap();
            gzip.finish().unwrap();
        }
        packed
    }

    fn listing_of(handle: IcFsHandle, path: &str) -> Vec<(String, bool, u64)> {
        let wanted = CString::new(path).unwrap();
        let listing = fs_list(handle, wanted.as_ptr());
        listing
            .as_slice()
            .iter()
            .map(|entry| (entry.name_string(), entry.is_directory(), entry.size))
            .collect()
    }

    fn contents_of(handle: IcFsHandle, path: &str) -> Vec<u8> {
        let wanted = CString::new(path).unwrap();
        fs_read(handle, wanted.as_ptr()).as_slice().to_vec()
    }

    #[test]
    fn a_tar_gz_lists_and_reads_like_a_zip_does() {
        let bytes = sample_tar_gz();
        let handle = open(&bytes, "sample.tar.gz");
        assert!(!handle.is_null(), "a gzipped tar opens");
        let root = listing_of(handle, "");
        let names: Vec<&str> = root.iter().map(|(name, _, _)| name.as_str()).collect();
        assert_eq!(names, vec!["deep", "notes.txt"]);
        assert!(root[0].1, "deep is a folder");
        assert!(!root[1].1);
        assert_eq!(
            contents_of(handle, "notes.txt"),
            b"tar contents".to_vec(),
            "a tar member reads back byte for byte"
        );
        fs_close(handle);
    }

    #[test]
    fn a_folder_nested_two_levels_deep_lists_its_own_contents() {
        let bytes = sample_tar_gz();
        let handle = open(&bytes, "sample.tar.gz");
        let first = listing_of(handle, "deep");
        assert_eq!(
            first
                .iter()
                .map(|(name, _, _)| name.as_str())
                .collect::<Vec<_>>(),
            vec!["one"]
        );
        assert!(first[0].1, "one is a folder");
        let second = listing_of(handle, "deep/one");
        assert_eq!(
            second
                .iter()
                .map(|(name, _, _)| name.as_str())
                .collect::<Vec<_>>(),
            vec!["two.txt"]
        );
        assert!(!second[0].1);
        assert_eq!(contents_of(handle, "deep/one/two.txt"), b"buried".to_vec());
        fs_close(handle);
    }

    #[test]
    fn a_listing_reports_the_size_of_each_file_and_none_for_a_folder() {
        let bytes = sample_zip();
        let handle = open(&bytes, "sample.zip");
        let root = listing_of(handle, "");
        let readme = root
            .iter()
            .find(|(name, _, _)| name == "readme.txt")
            .expect("the file is listed");
        assert_eq!(readme.2, b"hello archive".len() as u64, "the real size");
        let docs = root
            .iter()
            .find(|(name, _, _)| name == "docs")
            .expect("the folder is listed");
        assert_eq!(docs.2, 0, "a folder has no size of its own");
        let inner = listing_of(handle, "docs");
        assert_eq!(inner[0].2, b"inner file".len() as u64);
        fs_close(handle);
    }

    #[test]
    fn a_zip_lists_its_root_as_a_folder_would() {
        let bytes = sample_zip();
        let h = open(&bytes, "sample.zip");
        assert!(!h.is_null());
        let root = CString::new("").unwrap();
        let listing = fs_list(h, root.as_ptr());
        let names: Vec<String> = listing.as_slice().iter().map(|e| e.name_string()).collect();
        assert_eq!(names, vec!["docs".to_string(), "readme.txt".to_string()]);
        assert!(listing.as_slice()[0].is_directory());
        assert!(!listing.as_slice()[1].is_directory());
        fs_close(h);
    }

    #[test]
    fn descending_into_a_folder_shows_its_own_files() {
        let bytes = sample_zip();
        let h = open(&bytes, "sample.zip");
        let inner = CString::new("docs").unwrap();
        let listing = fs_list(h, inner.as_ptr());
        let names: Vec<String> = listing.as_slice().iter().map(|e| e.name_string()).collect();
        assert_eq!(names, vec!["inner.txt".to_string()]);
        fs_close(h);
    }

    #[test]
    fn a_file_reads_back_byte_for_byte() {
        let bytes = sample_zip();
        let h = open(&bytes, "sample.zip");
        let path = CString::new("docs/inner.txt").unwrap();
        let got = fs_read(h, path.as_ptr());
        assert_eq!(got.as_slice(), b"inner file");
        fs_close(h);
    }

    #[test]
    fn reading_something_that_is_not_there_reports_a_reason() {
        let bytes = sample_zip();
        let h = open(&bytes, "sample.zip");
        let path = CString::new("nope.txt").unwrap();
        let got = fs_read(h, path.as_ptr());
        assert!(got.as_slice().is_empty());
        let err = fs_last_error(h);
        assert!(!err.is_null());
        let text = unsafe { CStr::from_ptr(err) }.to_string_lossy().to_string();
        assert!(text.contains("not found"), "got: {text}");
        fs_close(h);
    }

    #[test]
    fn rubbish_does_not_open_at_all() {
        let junk = b"this is not an archive".to_vec();
        let h = open(&junk, "broken.zip");
        assert!(
            h.is_null(),
            "a bad archive must refuse to open, not crash later"
        );
    }

    #[test]
    fn an_archive_of_a_format_we_can_rewrite_is_not_read_only() {
        for (bytes, name) in [
            (sample_zip(), "sample.zip"),
            (sample_tar_gz(), "sample.tar.gz"),
        ] {
            let h = open(&bytes, name);
            assert_eq!(fs_is_read_only(h), 0, "{name} can be written to");
            fs_close(h);
        }
    }

    fn write_into(handle: IcFsHandle, path: &str, bytes: &[u8]) -> c_int {
        let wanted = CString::new(path).unwrap();
        fs_write(handle, wanted.as_ptr(), bytes.as_ptr(), bytes.len() as u64)
    }

    #[test]
    fn an_archive_opened_on_an_empty_file_becomes_a_real_one_when_written_to() {
        for name in ["fresh.zip", "fresh.tar.gz", "fresh.tar"] {
            let held = Somewhere::holding(b"", name);
            let handle = open_in(&held, name);
            assert!(!handle.is_null(), "{name} is made rather than refused");
            assert!(
                listing_of(handle, "").is_empty(),
                "{name} holds nothing yet"
            );
            assert_eq!(
                write_into(handle, "/first.txt", b"in"),
                ic_plugin_api::IC_OK,
                "{name} took the file"
            );
            assert_eq!(contents_of(handle, "first.txt"), b"in".to_vec());
            fs_close(handle);

            let whole = held.as_it_stands(name);
            assert!(
                !whole.is_empty(),
                "{name} was written out to the file it lives in"
            );
            let again = open_in(&held, name);
            assert!(!again.is_null(), "{name} reads back as an archive");
            assert_eq!(contents_of(again, "first.txt"), b"in".to_vec());
            fs_close(again);
        }
    }

    #[test]
    fn a_file_copied_into_an_archive_is_in_it_afterwards() {
        for (bytes, name) in [
            (sample_zip(), "sample.zip"),
            (sample_tar_gz(), "sample.tar.gz"),
        ] {
            let held = Somewhere::holding(&bytes, name);
            let handle = open_in(&held, name);
            assert_eq!(
                write_into(handle, "/added.txt", b"fresh"),
                ic_plugin_api::IC_OK,
                "{name} took the file"
            );
            assert_eq!(
                contents_of(handle, "added.txt"),
                b"fresh".to_vec(),
                "{name} reads back what was written"
            );
            let listed = listing_of(handle, "");
            assert!(
                listed.iter().any(|(held, _, _)| held == "added.txt"),
                "{name} lists it: {listed:?}"
            );

            fs_close(handle);
            let again = open_in(&held, name);
            assert_eq!(contents_of(again, "added.txt"), b"fresh".to_vec());
            fs_close(again);
        }
    }

    #[test]
    fn writing_over_a_file_that_is_already_there_replaces_it_rather_than_doubling_it() {
        let bytes = sample_zip();
        let handle = open(&bytes, "sample.zip");
        let first = listing_of(handle, "").len();
        assert_eq!(
            write_into(handle, "/readme.txt", b"again"),
            ic_plugin_api::IC_OK
        );
        assert_eq!(contents_of(handle, "readme.txt"), b"again".to_vec());
        assert_eq!(listing_of(handle, "").len(), first, "no second entry");
        fs_close(handle);
    }

    #[test]
    fn a_folder_made_inside_an_archive_is_there_and_takes_files() {
        let bytes = sample_zip();
        let handle = open(&bytes, "sample.zip");
        let made = CString::new("/new").unwrap();
        assert_eq!(fs_create_dir(handle, made.as_ptr()), ic_plugin_api::IC_OK);
        assert_eq!(
            fs_create_dir(handle, made.as_ptr()),
            ic_plugin_api::IC_ERR_IO,
            "making it twice is refused rather than silently doubling it"
        );
        assert_eq!(
            write_into(handle, "/new/inside.txt", b"in"),
            ic_plugin_api::IC_OK
        );
        let listed = listing_of(handle, "new");
        assert_eq!(
            listed
                .iter()
                .map(|(name, _, _)| name.as_str())
                .collect::<Vec<_>>(),
            vec!["inside.txt"]
        );
        fs_close(handle);
    }

    #[test]
    fn removing_a_folder_takes_what_was_under_it() {
        let bytes = sample_tar_gz();
        let handle = open(&bytes, "sample.tar.gz");
        assert_eq!(contents_of(handle, "deep/one/two.txt"), b"buried".to_vec());
        let gone = CString::new("/deep").unwrap();
        assert_eq!(fs_remove(handle, gone.as_ptr()), ic_plugin_api::IC_OK);
        let left = listing_of(handle, "");
        assert_eq!(
            left.iter()
                .map(|(name, _, _)| name.as_str())
                .collect::<Vec<_>>(),
            vec!["notes.txt"],
            "the folder and everything under it went"
        );
        assert_eq!(
            fs_remove(handle, gone.as_ptr()),
            ic_plugin_api::IC_ERR_IO,
            "removing what is not there says so"
        );
        fs_close(handle);
    }

    #[test]
    fn a_format_we_cannot_put_together_again_is_read_only_and_refuses_a_write() {
        let bytes = sample_zip();
        let handle = open(&bytes, "sample.zip");
        with_opened(handle, |o| o.packing = None);
        assert_eq!(fs_is_read_only(handle), 1);
        assert_eq!(
            write_into(handle, "/added.txt", b"fresh"),
            ic_plugin_api::IC_ERR_IO
        );
        fs_close(handle);
    }

    #[cfg(not(windows))]
    const HOSTILE: &[&str] = &[
        "../evil",
        "a/../../evil",
        "a/../evil",
        "/abs/evil",
        "..\\evil",
        "a\\..\\..\\evil",
        "\\\\server\\share\\evil",
        "//server/share/evil",
    ];

    #[cfg(windows)]
    const HOSTILE: &[&str] = &[
        "../evil",
        "a/../../evil",
        "a/../evil",
        "/abs/evil",
        "..\\evil",
        "a\\..\\..\\evil",
        "\\\\server\\share\\evil",
        "//server/share/evil",
        "C:\\evil",
        "C:/evil",
        "a/C:evil",
        "a:b.txt",
        "a/.. /evil",
    ];

    fn hostile_zip() -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            let opts = zip::write::FileOptions::default();
            for name in HOSTILE {
                zip.start_file(*name, opts).unwrap();
                zip.write_all(b"evil").unwrap();
            }
            zip.start_file("a/./b", opts).unwrap();
            zip.write_all(b"benign b").unwrap();
            zip.start_file("safe.txt", opts).unwrap();
            zip.write_all(b"benign safe").unwrap();
            zip.finish().unwrap();
        }
        buf
    }

    fn hostile_tar() -> Vec<u8> {
        let mut tar = Vec::new();
        {
            let mut writer = tar::Builder::new(&mut tar);
            let mut add = |path: &str, body: &[u8]| {
                let mut header = tar::Header::new_old();
                // set_path refuses `..` and absolute names, which is exactly what is wanted here.
                header.as_old_mut().name[..path.len()].copy_from_slice(path.as_bytes());
                header.set_size(body.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                writer.append(&header, body).unwrap();
            };
            for name in HOSTILE {
                add(name, b"evil");
            }
            add("a/./b", b"benign b");
            add("safe.txt", b"benign safe");
            writer.finish().unwrap();
        }
        tar
    }

    fn compressed(tar: &[u8], name: &str) -> Vec<u8> {
        if name.ends_with(".tar") {
            return tar.to_vec();
        }
        let mut out = Vec::new();
        if name.ends_with(".tar.gz") {
            let mut gzip = flate2::write::GzEncoder::new(&mut out, flate2::Compression::default());
            gzip.write_all(tar).unwrap();
            gzip.finish().unwrap();
        } else {
            let mut bz = bzip2::write::BzEncoder::new(&mut out, bzip2::Compression::default());
            bz.write_all(tar).unwrap();
            bz.finish().unwrap();
        }
        out
    }

    fn hostile_archives() -> Vec<(Vec<u8>, &'static str)> {
        let tar = hostile_tar();
        vec![
            (hostile_zip(), "hostile.zip"),
            (compressed(&tar, "hostile.tar"), "hostile.tar"),
            (compressed(&tar, "hostile.tar.gz"), "hostile.tar.gz"),
            (compressed(&tar, "hostile.tar.bz2"), "hostile.tar.bz2"),
        ]
    }

    fn raw_names(bytes: &[u8], name: &str) -> Vec<String> {
        if is_tar_format_from_name(name) {
            let mut archive = tar::Archive::new(tar_reader_mem(bytes));
            archive
                .entries()
                .unwrap()
                .map(|entry| entry.unwrap().path().unwrap().to_string_lossy().to_string())
                .collect()
        } else {
            let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
            (0..archive.len())
                .map(|at| archive.by_index(at).unwrap().name().to_string())
                .collect()
        }
    }

    fn copied_as_the_host_would(handle: IcFsHandle) -> Vec<(String, Vec<u8>)> {
        let mut found = Vec::new();
        let mut queue = vec![String::new()];
        while let Some(dir) = queue.pop() {
            let wanted = CString::new(dir.clone()).unwrap();
            for row in fs_list_rows(handle, wanted.as_ptr()).as_slice() {
                let name = row.entry.name_string();
                let path = if dir.is_empty() {
                    name.clone()
                } else {
                    format!("{dir}/{name}")
                };
                if row.entry.is_directory() {
                    queue.push(path);
                } else {
                    let bytes = contents_of(handle, &path);
                    found.push((path, bytes));
                }
            }
        }
        found.sort();
        found
    }

    #[test]
    fn no_entry_that_climbs_out_of_the_archive_is_listed_or_read() {
        assert!(raw_names(&hostile_tar(), "x.tar").contains(&"../evil".to_string()));
        for (bytes, name) in hostile_archives() {
            let handle = open(&bytes, name);
            assert!(!handle.is_null(), "{name} opens");
            assert_eq!(
                copied_as_the_host_would(handle),
                vec![
                    ("a/b".to_string(), b"benign b".to_vec()),
                    ("safe.txt".to_string(), b"benign safe".to_vec()),
                ],
                "{name}"
            );
            let names: Vec<String> = listing_of(handle, "")
                .into_iter()
                .map(|(entry, _, _)| entry)
                .collect();
            assert_eq!(names, vec!["a", "safe.txt"], "{name}");
            assert_eq!(contents_of(handle, "a/./b"), b"benign b".to_vec(), "{name}");
            for hostile in HOSTILE {
                assert!(
                    contents_of(handle, hostile).is_empty(),
                    "{name}: {hostile} read"
                );
            }
            for climb in ["..", "a/..", "../..", "a/../..", "..\\", "C:", "a/.. "] {
                let wanted = CString::new(climb).unwrap();
                assert!(
                    listing_of(handle, climb).is_empty(),
                    "{name}: {climb} listed"
                );
                assert_eq!(
                    fs_list_rows(handle, wanted.as_ptr()).count,
                    0,
                    "{name}: {climb}"
                );
            }
            fs_close(handle);
        }
    }

    fn last_error_of(handle: IcFsHandle) -> String {
        unsafe { CStr::from_ptr(fs_last_error(handle)) }
            .to_string_lossy()
            .to_string()
    }

    #[test]
    fn nothing_can_be_written_outside_the_archive_and_hidden_entries_are_never_rewritten_away() {
        for (bytes, name) in hostile_archives() {
            let held = Somewhere::holding(&bytes, name);
            let handle = open_in(&held, name);
            let climbing = HOSTILE
                .iter()
                .filter(|hostile| !hostile.starts_with(['/', '\\']))
                .chain(["..", "a/..", "a/../.."].iter());
            for hostile in climbing {
                assert_eq!(
                    write_into(handle, hostile, b"evil"),
                    ic_plugin_api::IC_ERR_IO,
                    "{name}: {hostile}"
                );
                let wanted = CString::new(*hostile).unwrap();
                assert_eq!(
                    fs_create_dir(handle, wanted.as_ptr()),
                    ic_plugin_api::IC_ERR_IO,
                    "{name}: {hostile}"
                );
                assert_eq!(
                    fs_remove(handle, wanted.as_ptr()),
                    ic_plugin_api::IC_ERR_IO,
                    "{name}: {hostile}"
                );
            }
            let benign = CString::new("/a/./b").unwrap();
            assert_eq!(
                write_into(handle, "/a/./c", b"benign c"),
                ic_plugin_api::IC_ERR_IO,
                "{name}"
            );
            assert!(last_error_of(handle).contains("would lose them"), "{name}");
            assert_eq!(
                fs_create_dir(handle, c"/new".as_ptr()),
                ic_plugin_api::IC_ERR_IO
            );
            assert_eq!(fs_remove(handle, benign.as_ptr()), ic_plugin_api::IC_ERR_IO);
            assert_eq!(contents_of(handle, "a/b"), b"benign b".to_vec(), "{name}");
            fs_close(handle);

            assert_eq!(held.as_it_stands(name), bytes, "{name} was left alone");
            let beside: Vec<_> = std::fs::read_dir(&held.0)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect();
            assert_eq!(beside, vec![std::ffi::OsString::from(name)], "{name}");
        }
    }

    fn tar_of(names: &[(&str, &[u8])]) -> Vec<u8> {
        let mut tar = Vec::new();
        {
            let mut writer = tar::Builder::new(&mut tar);
            for (path, body) in names {
                let mut header = tar::Header::new_old();
                header.as_old_mut().name[..path.len()].copy_from_slice(path.as_bytes());
                header.set_size(body.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                writer.append(&header, *body).unwrap();
            }
            writer.finish().unwrap();
        }
        tar
    }

    fn zip_of(names: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            let opts = zip::write::FileOptions::default();
            for (path, body) in names {
                zip.start_file(*path, opts).unwrap();
                zip.write_all(body).unwrap();
            }
            zip.finish().unwrap();
        }
        buf
    }

    const ODD_NAMES: &[(&str, &[u8])] = &[
        ("...", b"dots"),
        ("a:b.txt", b"colon"),
        ("x/.../y", b"deep dots"),
        ("C:/z", b"drive"),
        ("w/.. /v", b"spaced"),
        ("kept.txt", b"kept"),
    ];

    #[cfg(not(windows))]
    #[test]
    fn names_that_are_fine_on_unix_are_shown_read_and_kept_by_a_rewrite() {
        let expected: Vec<(String, Vec<u8>)> = vec![
            ("...".to_string(), b"dots".to_vec()),
            ("C:/z".to_string(), b"drive".to_vec()),
            ("a:b.txt".to_string(), b"colon".to_vec()),
            ("kept.txt".to_string(), b"kept".to_vec()),
            ("w/.. /v".to_string(), b"spaced".to_vec()),
            ("x/.../y".to_string(), b"deep dots".to_vec()),
        ];
        for (bytes, name) in [
            (zip_of(ODD_NAMES), "odd.zip"),
            (tar_of(ODD_NAMES), "odd.tar"),
        ] {
            let held = Somewhere::holding(&bytes, name);
            let handle = open_in(&held, name);
            assert_eq!(copied_as_the_host_would(handle), expected, "{name}");
            assert_eq!(
                write_into(handle, "/new.txt", b"x"),
                ic_plugin_api::IC_OK,
                "{name}"
            );
            fs_close(handle);

            let again = open_in(&held, name);
            let mut grown = expected.clone();
            grown.push(("new.txt".to_string(), b"x".to_vec()));
            grown.sort();
            assert_eq!(copied_as_the_host_would(again), grown, "{name}");
            fs_close(again);
        }
    }

    #[cfg(windows)]
    #[test]
    fn drives_and_spaced_climbs_are_hidden_on_windows_and_block_a_rewrite() {
        for (bytes, name) in [
            (zip_of(ODD_NAMES), "odd.zip"),
            (tar_of(ODD_NAMES), "odd.tar"),
        ] {
            let held = Somewhere::holding(&bytes, name);
            let handle = open_in(&held, name);
            assert_eq!(
                copied_as_the_host_would(handle),
                vec![
                    ("...".to_string(), b"dots".to_vec()),
                    ("kept.txt".to_string(), b"kept".to_vec()),
                    ("x/.../y".to_string(), b"deep dots".to_vec()),
                ],
                "{name}"
            );
            assert!(contents_of(handle, "a:b.txt").is_empty(), "{name}");
            assert_eq!(
                write_into(handle, "/new.txt", b"x"),
                ic_plugin_api::IC_ERR_IO,
                "{name}"
            );
            fs_close(handle);
            assert_eq!(held.as_it_stands(name), bytes, "{name}");
        }
    }

    #[test]
    fn two_entries_that_normalize_to_one_name_show_the_last_and_block_a_rewrite() {
        let mut zip_bytes = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut zip_bytes));
            let opts = zip::write::FileOptions::default();
            for (path, body) in [("./docs/a.txt", &b"first"[..]), ("docs/a.txt", b"second")] {
                zip.start_file(path, opts).unwrap();
                zip.write_all(body).unwrap();
            }
            zip.add_directory("docs/", opts).unwrap();
            zip.add_directory("./docs/", opts).unwrap();
            zip.finish().unwrap();
        }
        let tar = tar_of(&[("docs/a.txt", b"first"), ("docs//a.txt", b"second")]);
        for (bytes, name) in [(zip_bytes, "twice.zip"), (tar, "twice.tar")] {
            let held = Somewhere::holding(&bytes, name);
            let handle = open_in(&held, name);
            assert_eq!(
                copied_as_the_host_would(handle),
                vec![("docs/a.txt".to_string(), b"second".to_vec())],
                "{name}"
            );
            assert_eq!(listing_of(handle, "docs")[0].2, 6, "{name}");
            assert_eq!(
                write_into(handle, "/b.txt", b"x"),
                ic_plugin_api::IC_ERR_IO,
                "{name}"
            );
            fs_close(handle);
            assert_eq!(held.as_it_stands(name), bytes, "{name}");
        }
    }

    #[test]
    fn a_file_appended_again_shows_its_last_version_as_after_tar_r() {
        let first = tar_of(&[("notes.txt", b"v1"), ("other.txt", b"other")]);
        let mut appended = first[..first.len() - 1024].to_vec();
        appended.extend(tar_of(&[("notes.txt", b"version two")]));
        let mut zip_bytes = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut zip_bytes));
            let opts = zip::write::FileOptions::default();
            for (path, body) in [
                ("notes.txt", &b"v1"[..]),
                ("other.txt", b"other"),
                ("notes.txt", b"version two"),
            ] {
                zip.start_file(path, opts).unwrap();
                zip.write_all(body).unwrap();
            }
            zip.finish().unwrap();
        }
        for (bytes, name) in [
            (compressed(&appended, "r.tar"), "r.tar"),
            (compressed(&appended, "r.tar.gz"), "r.tar.gz"),
            (compressed(&appended, "r.tar.bz2"), "r.tar.bz2"),
            (zip_bytes, "r.zip"),
        ] {
            assert_eq!(
                raw_names(&bytes, name),
                vec!["notes.txt", "other.txt", "notes.txt"],
                "{name}"
            );
            let held = Somewhere::holding(&bytes, name);
            let handle = open_in(&held, name);
            assert_eq!(
                listing_of(handle, ""),
                vec![
                    ("notes.txt".to_string(), false, 11),
                    ("other.txt".to_string(), false, 5),
                ],
                "{name}"
            );
            assert_eq!(contents_of(handle, "notes.txt"), b"version two", "{name}");
            assert_eq!(
                write_into(handle, "/other.txt", b"x"),
                ic_plugin_api::IC_ERR_IO,
                "{name}"
            );
            assert!(last_error_of(handle).contains("1 entries"), "{name}");
            fs_close(handle);
            assert_eq!(held.as_it_stands(name), bytes, "{name}");
        }
    }

    #[test]
    fn a_repeated_folder_alone_does_not_block_a_rewrite() {
        let mut zip_bytes = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut zip_bytes));
            let opts = zip::write::FileOptions::default();
            zip.add_directory("docs/", opts).unwrap();
            zip.add_directory("./docs/", opts).unwrap();
            zip.finish().unwrap();
        }
        let handle = open(&zip_bytes, "folders.zip");
        assert_eq!(
            write_into(handle, "/docs/x.txt", b"x"),
            ic_plugin_api::IC_OK
        );
        assert_eq!(contents_of(handle, "docs/x.txt"), b"x".to_vec());
        fs_close(handle);
    }

    #[test]
    fn a_cut_short_tar_still_reads_what_it_lists_and_refuses_a_rewrite() {
        let whole = tar_of(&[("first.txt", b"first"), ("second.bin", &[7u8; 4096])]);
        let cut = whole[..512 + 512 + 512 + 1000].to_vec();
        let held = Somewhere::holding(&cut, "cut.tar");
        let handle = open_in(&held, "cut.tar");
        assert_eq!(contents_of(handle, "first.txt"), b"first".to_vec());
        assert_eq!(write_into(handle, "/n.txt", b"x"), ic_plugin_api::IC_ERR_IO);
        fs_close(handle);
        assert_eq!(held.as_it_stands("cut.tar"), cut);
    }

    #[test]
    fn a_zip_entry_that_cannot_be_opened_does_not_shadow_the_one_listed() {
        let mut bytes = zip_of(&[("a.txt", b"one"), ("a.txt", b"two")]);
        let second_local = bytes
            .windows(4)
            .enumerate()
            .filter(|(_, sig)| *sig == b"PK\x03\x04")
            .nth(1)
            .unwrap()
            .0;
        let second_central = bytes
            .windows(4)
            .enumerate()
            .filter(|(_, sig)| *sig == b"PK\x01\x02")
            .nth(1)
            .unwrap()
            .0;
        bytes[second_local + 6] |= 1;
        bytes[second_central + 8] |= 1;
        let held = Somewhere::holding(&bytes, "locked.zip");
        let handle = open_in(&held, "locked.zip");
        assert_eq!(
            listing_of(handle, ""),
            vec![("a.txt".to_string(), false, 3)]
        );
        assert_eq!(contents_of(handle, "a.txt"), b"one".to_vec());
        assert_eq!(write_into(handle, "/n.txt", b"x"), ic_plugin_api::IC_ERR_IO);
        fs_close(handle);
        assert_eq!(held.as_it_stands("locked.zip"), bytes);
    }

    #[test]
    fn links_and_a_folder_overwritten_by_a_file_block_a_rewrite() {
        let mut linked = Vec::new();
        {
            let mut writer = tar::Builder::new(&mut linked);
            let mut header = tar::Header::new_gnu();
            header.set_size(4);
            header.set_mode(0o644);
            header.set_cksum();
            writer
                .append_data(&mut header, "real.txt", &b"real"[..])
                .unwrap();
            for (name, kind) in [
                ("link", tar::EntryType::Symlink),
                ("hard", tar::EntryType::Link),
            ] {
                let mut header = tar::Header::new_gnu();
                header.set_entry_type(kind);
                header.set_size(0);
                writer.append_link(&mut header, name, "real.txt").unwrap();
            }
            writer.finish().unwrap();
        }
        let mut shadowed = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut shadowed));
            let opts = zip::write::FileOptions::default();
            zip.add_directory("docs/", opts).unwrap();
            zip.start_file("docs", opts).unwrap();
            zip.write_all(b"file").unwrap();
            zip.finish().unwrap();
        }
        for (bytes, name) in [(linked, "links.tar"), (shadowed, "shadowed.zip")] {
            let held = Somewhere::holding(&bytes, name);
            let handle = open_in(&held, name);
            assert_eq!(
                write_into(handle, "/n.txt", b"x"),
                ic_plugin_api::IC_ERR_IO,
                "{name}"
            );
            fs_close(handle);
            assert_eq!(held.as_it_stands(name), bytes, "{name}");
        }
    }

    #[test]
    fn a_rewrite_keeps_the_permissions_and_times_it_found() {
        let mut tar = Vec::new();
        {
            let mut writer = tar::Builder::new(&mut tar);
            let mut header = tar::Header::new_gnu();
            header.set_size(2);
            header.set_mode(0o750);
            header.set_mtime(1_600_000_000);
            header.set_cksum();
            writer
                .append_data(&mut header, "run.sh", &b"#!"[..])
                .unwrap();
            writer.finish().unwrap();
        }
        let mut zip_bytes = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut zip_bytes));
            let opts = zip::write::FileOptions::default()
                .unix_permissions(0o750)
                .last_modified_time(
                    zip::DateTime::from_date_and_time(2020, 9, 13, 12, 26, 40).unwrap(),
                );
            zip.start_file("run.sh", opts).unwrap();
            zip.write_all(b"#!").unwrap();
            zip.finish().unwrap();
        }
        for (bytes, name) in [(tar, "kept.tar"), (zip_bytes, "kept.zip")] {
            let held = Somewhere::holding(&bytes, name);
            let handle = open_in(&held, name);
            let before = fs_list(handle, c"".as_ptr()).as_slice()[0];
            assert_eq!(
                write_into(handle, "/n.txt", b"x"),
                ic_plugin_api::IC_OK,
                "{name}"
            );
            fs_close(handle);
            let again = open_in(&held, name);
            let rows = fs_list(again, c"".as_ptr());
            let after = rows
                .as_slice()
                .iter()
                .find(|row| row.name_string() == "run.sh")
                .copied()
                .unwrap();
            assert_eq!(after.permissions & 0o777, 0o750, "{name}");
            assert_eq!(after.modified, before.modified, "{name}");
            fs_close(again);
        }
    }

    #[test]
    fn a_tar_never_hands_out_setuid_setgid_or_sticky() {
        let mut tar = Vec::new();
        {
            let mut writer = tar::Builder::new(&mut tar);
            let mut header = tar::Header::new_gnu();
            header.set_path("run").unwrap();
            header.set_size(2);
            header.set_mode(0o7755);
            header.set_cksum();
            writer.append(&header, &b"#!"[..]).unwrap();
            writer.finish().unwrap();
        }
        let handle = open(&tar, "modes.tar");
        let rows = fs_list(handle, c"".as_ptr());
        assert_eq!(rows.as_slice()[0].permissions, 0o755);
        fs_close(handle);
    }

    #[test]
    fn packing_refuses_a_path_that_is_not_plainly_inside() {
        for hostile in HOSTILE.iter().chain(["a/./b", "", "/", "."].iter()) {
            for packing in [
                pack::Packing::Zip,
                pack::Packing::Tar,
                pack::Packing::TarGz,
                pack::Packing::TarBz2,
            ] {
                let entry = pack::Entry {
                    path: hostile.to_string(),
                    is_dir: false,
                    bytes: b"evil".to_vec(),
                    permissions: None,
                    modified: None,
                };
                assert!(pack::pack(&[entry], packing).is_err(), "{hostile}");
            }
        }
    }

    #[test]
    fn a_plain_archive_is_untouched_by_the_rule() {
        for name in [
            "docs/inner.txt",
            "readme.txt",
            "a/b/c",
            "name with space.txt",
            "..x",
            "x..",
            ".hidden",
            "...",
            "a/.../b",
            " ..",
        ] {
            assert_eq!(enclosed(name).as_deref(), Some(name));
        }
        assert_eq!(enclosed("docs/").as_deref(), Some("docs"));
        assert_eq!(enclosed("a/./b").as_deref(), Some("a/b"));
        assert_eq!(enclosed("a//b").as_deref(), Some("a/b"));
        assert_eq!(enclosed("a\\b").as_deref(), Some("a/b"));
        for name in ["a:b.txt", "C:/x", "a/C:x", ".. ", "a/.. /b", ". "] {
            assert_eq!(enclosed(name).is_some(), cfg!(not(windows)), "{name}");
        }
        for hostile in HOSTILE {
            assert_eq!(enclosed(hostile), None, "{hostile}");
        }
    }

    #[test]
    fn the_extension_list_covers_what_we_claim() {
        for ext in [
            ".zip", ".tar", ".tar.gz", ".tgz", ".tar.bz2", ".tbz2", ".tbz",
        ] {
            assert!(EXTENSIONS.contains(ext), "{ext} missing");
        }
    }
}
