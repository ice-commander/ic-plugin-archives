# Archives

An Ice Commander plugin that opens zip and tar archives as folders in a panel.

## What it does

- Claims `.zip .tar .tar.gz .tgz .tar.bz2 .tbz2 .tbz`, matched case-insensitively
  on the file name.
- Lists an archive level by level, like a directory, with size, date and Unix
  permissions where the archive records them, and reads any file out of it.
- Zip or tar is decided by the name; for a tar the content is sniffed for gzip or
  bzip2 (multi-stream bzip2 included), otherwise it is read as a plain tar.
- Adds two columns, **Packed** and **Ratio** (compressed size and its share of the
  original). Both are empty for folders and for every tar format, which has no
  per-entry compressed size. The headings ship in 15 languages and are registered
  with the host at start-up.
- Copying a file in, creating a folder and deleting (a folder with everything under
  it) are supported. Each change repacks the archive in memory, writes it back
  through the filesystem it was opened on and reports the change to the host.
- An empty file with a claimed extension opens as an empty archive of that format;
  it is written out on the first change.

The plugin never receives the archive's bytes from the host: it gets a filesystem
handle and a path, and reads and writes the file through the host's `fs_*` calls.
The whole archive is held in memory while it is mounted.

## Building

```sh
./build.sh          # release build, libraries collected into bin/
./test.sh           # cargo test --workspace
./deploy-local.sh   # copies bin/* into the plugin folder (IC_PLUGIN_DIR overrides it)
```

`ic-plugin-api` is fetched from its git repository. Build output goes to
`bin/target`. After deploying, enable the plugin in **Settings → Plugins** and
restart.

## Known limitations

- Rename is not implemented (`rename` slot is `None`).
- No other formats: no 7z, rar, xz, zstd, or single-file `.gz`/`.bz2`.
- Every change rewrites the whole file. For tar formats each entry is located by
  re-reading the stream from the start, so a change costs time quadratic in the
  number of entries.
- Repacking does not keep entry metadata: tar entries get mtime 0 and mode
  0644/0755; zip entries get the current time, mode 0755 and Deflate.
- Tar symlinks and hard links are repacked as empty regular files.
- Zip entries the `zip` crate cannot open (encrypted, or an unsupported method such
  as LZMA) are left out of the listing. A file stored after such an entry cannot be
  read, and every change to the archive fails; only when the unreadable entries come
  last does a change go through, and it drops them.
- A tar-family file that cannot be parsed opens as an empty or partial listing
  instead of failing. An entry cut short at the end is listed but cannot be read,
  and every change then fails. When the cut falls between entries, or nothing parses
  at all, the first change rewrites the file with only what was listed.
- A file that cannot be opened for reading mounts as an empty archive, and the first
  change overwrites it. A read error part-way through is taken as the end of the
  file: a zip then fails to open, a tar-family file is treated as cut short.

## Licence

MIT or Apache-2.0, at your option. Contributions are taken under the DCO; sign
off with `git commit -s`.
