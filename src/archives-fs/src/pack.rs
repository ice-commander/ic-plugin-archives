pub struct Entry {
    pub path: String,
    pub is_dir: bool,
    pub bytes: Vec<u8>,
    pub permissions: Option<u32>,
    pub modified: Option<u64>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Packing {
    Zip,
    Tar,
    TarGz,
    TarBz2,
}

pub fn packing_for(name: &str) -> Option<Packing> {
    let lowered = name.to_lowercase();
    if lowered.ends_with(".zip") {
        Some(Packing::Zip)
    } else if lowered.ends_with(".tar.gz") || lowered.ends_with(".tgz") {
        Some(Packing::TarGz)
    } else if lowered.ends_with(".tar.bz2")
        || lowered.ends_with(".tbz2")
        || lowered.ends_with(".tbz")
    {
        Some(Packing::TarBz2)
    } else if lowered.ends_with(".tar") {
        Some(Packing::Tar)
    } else {
        None
    }
}

pub fn pack(entries: &[Entry], packing: Packing) -> Result<Vec<u8>, String> {
    if let Some(outside) = entries.iter().find(|entry| {
        let plain = entry.path.trim_end_matches('/');
        crate::enclosed(plain).as_deref() != Some(plain) || plain.is_empty()
    }) {
        return Err(format!("{} is not a path inside the archive", outside.path));
    }
    match packing {
        Packing::Zip => pack_zip(entries),
        Packing::Tar => pack_tar(entries, |body| Ok(body)),
        Packing::TarGz => pack_tar(entries, |body| {
            use std::io::Write;
            let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            enc.write_all(&body).map_err(|e| e.to_string())?;
            enc.finish().map_err(|e| e.to_string())
        }),
        Packing::TarBz2 => pack_tar(entries, |body| {
            use std::io::Write;
            let mut enc = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::default());
            enc.write_all(&body).map_err(|e| e.to_string())?;
            enc.finish().map_err(|e| e.to_string())
        }),
    }
}

fn pack_zip(entries: &[Entry]) -> Result<Vec<u8>, String> {
    use std::io::Write;
    let mut buffer = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buffer));
        let options =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for entry in entries {
            let mut options = options.unix_permissions(entry.permissions.unwrap_or(0o755));
            if let Some(stamp) = entry.modified.and_then(dos_time) {
                options = options.last_modified_time(stamp);
            }
            if entry.is_dir {
                zip.add_directory(format!("{}/", entry.path.trim_end_matches('/')), options)
                    .map_err(|e| e.to_string())?;
                continue;
            }
            zip.start_file(&entry.path, options)
                .map_err(|e| e.to_string())?;
            zip.write_all(&entry.bytes).map_err(|e| e.to_string())?;
        }
        zip.finish().map_err(|e| e.to_string())?;
    }
    Ok(buffer)
}

// The scan read the DOS time as local time, so it goes back the same way.
fn dos_time(stamp: u64) -> Option<zip::DateTime> {
    use chrono::{Datelike, TimeZone, Timelike};
    let local = chrono::Local
        .timestamp_opt(i64::try_from(stamp).ok()?, 0)
        .single()?;
    zip::DateTime::from_date_and_time(
        u16::try_from(local.year()).ok()?,
        local.month() as u8,
        local.day() as u8,
        local.hour() as u8,
        local.minute() as u8,
        local.second() as u8,
    )
    .ok()
}

fn pack_tar(
    entries: &[Entry],
    wrap: impl FnOnce(Vec<u8>) -> Result<Vec<u8>, String>,
) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut body);
        for entry in entries {
            let mut header = tar::Header::new_gnu();
            let fallback = if entry.is_dir { 0o755 } else { 0o644 };
            header.set_mode(entry.permissions.map_or(fallback, |mode| mode & 0o777));
            header.set_mtime(entry.modified.unwrap_or(0));
            if entry.is_dir {
                header.set_entry_type(tar::EntryType::Directory);
                header.set_size(0);
                builder
                    .append_data(
                        &mut header,
                        format!("{}/", entry.path.trim_end_matches('/')),
                        std::io::empty(),
                    )
                    .map_err(|e| e.to_string())?;
                continue;
            }
            header.set_size(entry.bytes.len() as u64);
            builder
                .append_data(&mut header, &entry.path, entry.bytes.as_slice())
                .map_err(|e| e.to_string())?;
        }
        builder.finish().map_err(|e| e.to_string())?;
    }
    wrap(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<Entry> {
        vec![
            Entry {
                path: "payload".to_string(),
                is_dir: true,
                bytes: Vec::new(),
                permissions: None,
                modified: None,
            },
            Entry {
                path: "payload/top.txt".to_string(),
                is_dir: false,
                bytes: b"top level".to_vec(),
                permissions: None,
                modified: None,
            },
            Entry {
                path: "payload/sub".to_string(),
                is_dir: true,
                bytes: Vec::new(),
                permissions: None,
                modified: None,
            },
            Entry {
                path: "payload/sub/deep.txt".to_string(),
                is_dir: false,
                bytes: b"nested content".to_vec(),
                permissions: None,
                modified: None,
            },
        ]
    }

    #[test]
    fn the_destination_name_chooses_the_packing() {
        assert!(matches!(packing_for("out.zip"), Some(Packing::Zip)));
        assert!(matches!(packing_for("OUT.ZIP"), Some(Packing::Zip)));
        assert!(matches!(packing_for("out.tar"), Some(Packing::Tar)));
        assert!(matches!(packing_for("out.tar.gz"), Some(Packing::TarGz)));
        assert!(matches!(packing_for("out.tgz"), Some(Packing::TarGz)));
        assert!(matches!(packing_for("out.tar.bz2"), Some(Packing::TarBz2)));
        assert!(matches!(packing_for("out.tbz"), Some(Packing::TarBz2)));
        assert!(packing_for("out.7z").is_none());
        assert!(packing_for("plainfile").is_none());
    }

    #[test]
    fn every_packing_reads_back_through_this_plugins_own_listing() {
        for name in ["out.zip", "out.tar", "out.tar.gz", "out.tar.bz2"] {
            let packed = pack(&sample(), packing_for(name).expect("a known packing"))
                .unwrap_or_else(|e| panic!("{name}: {e}"));

            let is_tar = crate::is_tar_format_from_name(name);
            let all = crate::scan_archive_from_memory(&packed, name, is_tar)
                .unwrap_or_else(|e| panic!("{name}: {e:?}"))
                .entries;
            let listing = crate::level(&all, "payload");
            assert!(
                listing.iter().any(|entry| entry.name == "top.txt"),
                "{name}: {listing:?}"
            );
            assert!(
                listing
                    .iter()
                    .any(|entry| entry.name == "sub" && entry.is_dir),
                "{name}: {listing:?}"
            );

            let deep =
                crate::read_file_from_archive_memory(&packed, "payload/sub/deep.txt", is_tar)
                    .unwrap_or_else(|e| panic!("{name}: {e:?}"));
            assert_eq!(deep, b"nested content", "{name}");
        }
    }

    #[test]
    fn a_tar_gz_is_smaller_than_the_plain_tar_for_repeated_bytes() {
        let bulky = vec![Entry {
            path: "big.txt".to_string(),
            is_dir: false,
            bytes: b"the same line over and over\n".repeat(400),
            permissions: None,
            modified: None,
        }];
        let plain = pack(&bulky, Packing::Tar).expect("tar");
        let zipped = pack(&bulky, Packing::TarGz).expect("tar.gz");
        assert!(
            zipped.len() < plain.len() / 4,
            "{} vs {}",
            zipped.len(),
            plain.len()
        );
    }
}
