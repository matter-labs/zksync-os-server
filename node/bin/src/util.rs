use flate2::read::GzDecoder;
use std::io::{BufReader, Read};
use std::path::Path;

/// Unpacks a tar archive (plain or gzip-compressed) into `dest`, stripping the single
/// top-level directory that the archive was created with — equivalent to GNU tar's
/// `--one-top-level=<dest>` option.
///
/// We detect gzip compression via magic bytes rather than file extension so that the
/// caller never has to worry about naming conventions.
pub fn unpack_ephemeral_state(archive_path: &Path, dest: &Path) {
    std::fs::create_dir_all(dest)
        .expect("failed to create destination directory for ephemeral state");
    let file =
        std::fs::File::open(archive_path).expect("ephemeral state archive exists and is readable");
    let mut probe = BufReader::new(file);

    // Read the first two bytes to detect gzip magic (0x1f 0x8b).
    let mut magic = [0u8; 2];
    probe
        .read_exact(&mut magic)
        .expect("ephemeral state archive is not empty");

    // Reopen the file so we can pass the full stream to the archive reader.  Using
    // `Seek::seek` would also work, but reopening is cleaner and avoids needing the
    // `Seek` bound on the reader.
    let file =
        std::fs::File::open(archive_path).expect("ephemeral state archive exists and is readable");

    fn unpack<R: Read>(reader: R, dest: &Path) {
        let mut archive = tar::Archive::new(reader);
        for entry in archive.entries().expect("valid tar archive") {
            let mut entry = entry.expect("valid tar entry");
            let entry_path = entry.path().expect("valid entry path").into_owned();

            // Strip the first path component to replicate `--one-top-level`: the
            // archive is expected to contain exactly one top-level directory (e.g.
            // `node/`), and we want its contents placed directly under `dest`.
            let stripped: std::path::PathBuf = entry_path.components().skip(1).collect();
            if stripped.as_os_str().is_empty() {
                // This is the top-level directory entry itself; skip it.
                continue;
            }

            let target = dest.join(&stripped);
            entry
                .unpack(&target)
                .unwrap_or_else(|e| panic!("failed to unpack {}: {e}", entry_path.display()));
        }
    }

    if magic == [0x1f, 0x8b] {
        // Gzip-compressed tar archive (.tar.gz / .tgz).
        unpack(GzDecoder::new(file), dest);
    } else {
        // Plain tar archive.
        unpack(file, dest);
    }

    tracing::info!(
        archive = %archive_path.display(),
        dest = %dest.display(),
        "Ephemeral state unpacked"
    );
}
