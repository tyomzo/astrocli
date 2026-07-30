//! The filesystem disciplines every write in this crate goes through.
//!
//! Nothing here is novel: SDD §5.11.2 argued each of these decisions once already for the
//! receiving side, and `astroctl-stack::mirror` implements them. This is the *sending* side of the
//! same layout, and it deliberately repeats the reasoning rather than weakening it:
//!
//!   * a temporary carries a **nonce**, because two overlapping attempts at one frame id would
//!     otherwise share a name and the loser would go on writing into a file descriptor the winner
//!     has already renamed into place — a stored raw modified after the fact, which REL-11 forbids;
//!   * a frame is linked with **`renameat2(RENAME_NOREPLACE)`**, because `metadata()`-then-`rename`
//!     has a window in which a concurrent retry lands and `rename(2)` replaces it silently. The
//!     `EEXIST` this returns instead is not a nuisance, it is the crash-recovery signal;
//!   * a **directory is fsynced** after a rename into it, because the file's contents were made
//!     durable before the rename but the name that makes them findable lives in the directory.
//!
//! The duplication with `astroctl-stack::mirror` is a considered cost. ADD §5.6 fixes the
//! dependency matrix, `crates/astroctl-session/testdata/session-layout.txt` states outright that
//! the two crates share *data*, not an edge, and taking a crate dependency to save a hundred lines
//! of syscall discipline would buy that saving with an architectural rule. If a third writer of
//! this layout ever appears, the answer is a module in `astroctl-core`, not an edge between peers.

use std::io;
use std::path::Path;

use sha2::{Digest as _, Sha256};
use tokio::io::AsyncWriteExt as _;

/// Marks a file that is mid-write and belongs to nobody until it is renamed.
///
/// The same spelling the mirror sweeps for (SDD §5.11.2), so an operator looking at either node's
/// session tree sees one kind of leftover rather than two.
pub(crate) const TMP_PREFIX: &str = ".tmp_";

/// Whether [`link_no_replace`] was the call that put the file there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Linked {
    /// This call created the name.
    Stored,
    /// Something already held the name; nothing was written and nothing was replaced.
    AlreadyPresent,
}

/// `renameat2(RENAME_NOREPLACE)` — link `from` to `to`, atomically refusing to overwrite.
pub(crate) async fn link_no_replace(from: &Path, to: &Path) -> io::Result<Linked> {
    let (from, to) = (from.to_owned(), to.to_owned());
    let result = tokio::task::spawn_blocking(move || {
        rustix::fs::renameat_with(
            rustix::fs::CWD,
            &from,
            rustix::fs::CWD,
            &to,
            rustix::fs::RenameFlags::NOREPLACE,
        )
    })
    .await
    .map_err(io::Error::other)?;

    match result {
        Ok(()) => Ok(Linked::Stored),
        Err(rustix::io::Errno::EXIST) => Ok(Linked::AlreadyPresent),
        Err(errno) => Err(io::Error::from(errno)),
    }
}

/// fsync a directory, so a rename into it survives a power cut.
pub(crate) async fn fsync_dir(dir: &Path) -> io::Result<()> {
    tokio::fs::File::open(dir).await?.sync_all().await
}

/// Write `bytes` to `path` so a reader sees either the old file or the new one, never a half.
///
/// tmp → fsync → rename → fsync the directory, exactly as SDD §5.5 requires for `session.json` and
/// the quality sidecars. A plain `rename` rather than the frames' `RENAME_NOREPLACE`: replacing
/// these files with a newer version is the intended operation, and the write-once rule of REL-11
/// is about raw frames.
///
/// The temporary is nonce'd by the caller through `tag`, for the reason in the module docs.
pub(crate) async fn write_atomic(path: &Path, tag: u64, bytes: &[u8]) -> io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no directory"))?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?;
    let tmp = dir.join(format!(
        "{TMP_PREFIX}{name}.{pid}.{tag}",
        pid = std::process::id()
    ));

    if let Err(error) = write_and_sync(&tmp, bytes).await {
        remove_quietly(&tmp).await;
        return Err(error);
    }
    if let Err(error) = tokio::fs::rename(&tmp, path).await {
        remove_quietly(&tmp).await;
        return Err(error);
    }
    fsync_dir(dir).await
}

/// Create `path`, write every byte, and make them durable before anyone can see the name.
async fn write_and_sync(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = tokio::fs::File::create(path).await?;
    file.write_all(bytes).await?;
    file.sync_all().await
}

/// Atomically point `link` at `target`, replacing whatever it pointed at.
///
/// `symlink` cannot overwrite, so the swap goes through a temporary of its own — which is also what
/// makes it atomic: a reader either follows the old target or the new one, and never finds `CURRENT`
/// missing (SDD §5.5 makes it the answer to "which session is live").
pub(crate) async fn symlink_atomic(link: &Path, target: &str, tag: u64) -> io::Result<()> {
    let dir = link
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "link has no directory"))?;
    let name = link
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "link has no file name"))?;
    let tmp = dir.join(format!(
        "{TMP_PREFIX}{name}.{pid}.{tag}",
        pid = std::process::id()
    ));

    remove_quietly(&tmp).await;
    tokio::fs::symlink(target, &tmp).await?;
    if let Err(error) = tokio::fs::rename(&tmp, link).await {
        remove_quietly(&tmp).await;
        return Err(error);
    }
    fsync_dir(dir).await
}

/// Lowercase hex SHA-256 of a file, and its size.
///
/// One `spawn_blocking` around the whole read rather than `tokio::fs` chunk by chunk, and the
/// reason is arithmetic: a Pi 4 has no SHA extensions and hashes at roughly 60 MB/s, so a 25 MB raw
/// is ~400 ms of pure CPU. Streamed through `tokio::fs` that lands on a runtime worker in 16 ms
/// bites, which is a scheduling stall on the two-worker runtime of SDD §7. In a blocking task it is
/// 400 ms on a thread that exists to be blocked.
pub(crate) async fn hash_and_size(path: &Path) -> io::Result<(String, u64)> {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || {
        let mut file = std::fs::File::open(&path)?;
        let mut hasher = Sha256::new();
        let mut buffer = vec![0_u8; 1 << 20];
        let mut size = 0_u64;
        loop {
            let read = std::io::Read::read(&mut file, &mut buffer)?;
            if read == 0 {
                break;
            }
            size += read as u64;
            hasher.update(&buffer[..read]);
        }
        Ok((hex(&hasher.finalize()), size))
    })
    .await
    .map_err(io::Error::other)?
}

/// Remove every `.tmp_*` entry directly in `dir`; returns how many went.
///
/// Nothing else ever removes them: a process that dies between `begin_frame` and `commit_frame`
/// leaks its partial frame, and on a field node those accumulate until the REL-12 threshold fires
/// months later with no explanation attached.
pub(crate) async fn sweep_temporaries_in(dir: &Path) -> u64 {
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return 0;
    };
    let mut removed = 0;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if !is_temporary(&path) {
            continue;
        }
        if tokio::fs::remove_file(&path).await.is_ok() {
            tracing::info!(path = %path.display(), "removed a temporary left by an interrupted capture");
            removed += 1;
        }
    }
    removed
}

/// Whether a path is one of this crate's in-flight temporaries.
pub(crate) fn is_temporary(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with(TMP_PREFIX))
}

/// Best-effort removal: the caller is already on a failure path and has nothing better to do with
/// a second error than log it.
pub(crate) async fn remove_quietly(path: &Path) {
    if let Err(error) = tokio::fs::remove_file(path).await {
        if error.kind() != io::ErrorKind::NotFound {
            tracing::warn!(path = %path.display(), %error, "cannot remove a temporary");
        }
    }
}

/// Create `dir` and every missing parent, fsyncing `dir`'s parent so the new name is durable.
///
/// A directory entry that is not fsynced can vanish in a power cut with an fsynced frame still
/// inside it — unreachable, and already reported as saved.
pub(crate) async fn create_dir_durable(dir: &Path) -> io::Result<()> {
    tokio::fs::create_dir_all(dir).await?;
    match dir.parent() {
        Some(parent) => fsync_dir(parent).await,
        None => Ok(()),
    }
}

/// Lowercase hex, the spelling `frame.saved` already uses (SDD §4.3).
pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        // Writing to a `String` cannot fail; the result is discarded rather than unwrapped so a
        // formatting quirk can never take the node down mid-capture.
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;

    #[tokio::test]
    async fn a_write_lands_whole_and_leaves_no_temporary() {
        let dir = TempDir::new();
        let path = dir.path().join("session.json");

        write_atomic(&path, 0, b"{\"v\":1}").await.unwrap();
        write_atomic(&path, 1, b"{\"v\":1,\"n\":2}").await.unwrap();

        assert_eq!(tokio::fs::read(&path).await.unwrap(), b"{\"v\":1,\"n\":2}");
        let mut entries = tokio::fs::read_dir(dir.path()).await.unwrap();
        while let Some(entry) = entries.next_entry().await.unwrap() {
            assert!(!is_temporary(&entry.path()), "{:?} survived", entry.path());
        }
    }

    /// The REL-11 primitive: the second link never wins, and the loser learns it lost.
    #[tokio::test]
    async fn linking_over_an_existing_name_is_refused_rather_than_silent() {
        let dir = TempDir::new();
        let (first, second, dest) = (
            dir.path().join("a.tmp"),
            dir.path().join("b.tmp"),
            dir.path().join("light_00001.cr3"),
        );
        tokio::fs::write(&first, b"first").await.unwrap();
        tokio::fs::write(&second, b"second").await.unwrap();

        assert_eq!(
            link_no_replace(&first, &dest).await.unwrap(),
            Linked::Stored
        );
        assert_eq!(
            link_no_replace(&second, &dest).await.unwrap(),
            Linked::AlreadyPresent
        );
        assert_eq!(tokio::fs::read(&dest).await.unwrap(), b"first");
    }

    #[tokio::test]
    async fn the_symlink_swap_never_leaves_the_link_missing() {
        let dir = TempDir::new();
        let link = dir.path().join("CURRENT");

        symlink_atomic(&link, "2026-07-30_ngc7000", 0)
            .await
            .unwrap();
        assert_eq!(
            tokio::fs::read_link(&link).await.unwrap(),
            Path::new("2026-07-30_ngc7000")
        );

        symlink_atomic(&link, "2026-07-31_m31", 1).await.unwrap();
        assert_eq!(
            tokio::fs::read_link(&link).await.unwrap(),
            Path::new("2026-07-31_m31")
        );
    }

    #[tokio::test]
    async fn hashing_reports_the_bytes_and_the_size() {
        let dir = TempDir::new();
        let path = dir.path().join("frame.cr3");
        tokio::fs::write(&path, b"abc").await.unwrap();

        let (sha256, size) = hash_and_size(&path).await.unwrap();

        assert_eq!(
            sha256,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(size, 3);
    }

    #[tokio::test]
    async fn the_sweep_removes_temporaries_and_nothing_else() {
        let dir = TempDir::new();
        tokio::fs::write(dir.path().join(".tmp_light_00001.7.0"), b"half")
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("light_00001.cr3"), b"whole")
            .await
            .unwrap();

        assert_eq!(sweep_temporaries_in(dir.path()).await, 1);
        assert!(dir.path().join("light_00001.cr3").exists());
    }

    #[test]
    fn hex_is_lowercase_and_zero_padded() {
        assert_eq!(hex(&[0x00, 0x0f, 0xff]), "000fff");
    }
}
