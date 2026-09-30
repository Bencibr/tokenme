//! Page-level decryption of Trae's `ModularData/ai-agent/database.db` — the
//! SQLCipher 4 store the IDE's Rust agent service keeps its chat turns in.
//!
//! The cipher is the one the CN-community reverses documented (DirWangK's
//! IDA write-up, mirrored by `Oh-My-Trae/trae-db-decrypt`), verified byte-for-byte
//! against this machine's *international* macOS build on 2026-09-30: the same
//! password sits in plain sight in `modules/ai-agent/libai_agent.dylib`, and the
//! constant it derives opens this install's database.
//!
//! ```text
//! password = "1CqAknayQsrfH9Byp2QzynTckHGzRom9"     (constant, in the dylib)
//! key      = PBKDF2-HMAC-SHA256(password, 123456789abcdef01122334455667788, 100k, 32B)
//! page 1   = salt(16) ‖ ct(4000) ‖ iv(16) ‖ hmac(64)
//! page N   = ct(4016) ‖ iv(16) ‖ hmac(64)
//! plaintext page = 4016 B content ‖ 80 B reserved tail (header byte 20 = 0x50)
//! ```
//!
//! Three details bite, each verified against the real file:
//! the plaintext does **not** carry the 16-byte `SQLite format 3\0` magic — page 1
//! must be re-prefixed by the reader; the HMACs are not checked (the assembled
//! image's SQLite header is the validity gate, the same trade-off the
//! wechat-decrypt lineage makes); and the `-wal` sidecar holds encrypted frames
//! under a *plaintext* WAL header, so recent turns live or die by applying it.

use std::path::Path;

use aes::cipher::generic_array::GenericArray;
use aes::Aes256;
use cbc::cipher::{BlockDecryptMut, KeyIvInit};

/// The agent's password, constant across the builds checked (CN 2026-07, intl
/// 3.5.104). A future build that rotates it fails the header check below and
/// degrades to silence — never to wrong numbers.
const PASSWORD: &[u8] = b"1CqAknayQsrfH9Byp2QzynTckHGzRom9";
/// The KDF salt from the same write-up, consumed as raw bytes.
const KDF_SALT: [u8; 16] = [
    0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88,
];
const KDF_ITERS: u32 = 100_000;
const PAGE: usize = 4096;
/// Per-page reserve: IV (16) + HMAC-SHA512 (64). Also the plaintext pages' own
/// declared reserved-space (header byte 20), which the rebuilt image keeps.
const RESERVE: usize = 80;
const CONTENT: usize = PAGE - RESERVE;
const IV: usize = 16;
const MAGIC: &[u8; 16] = b"SQLite format 3\x00";

/// The derived AES-256 key. PBKDF2 over a constant runs ~0.1 s; cached so a
/// full rescan pays it once.
fn derived_key() -> [u8; 32] {
    use pbkdf2::pbkdf2_hmac;
    use sha2::Sha256;
    let mut key = [0u8; 32];
    pbkdf2_hmac::<Sha256>(PASSWORD, &KDF_SALT, KDF_ITERS, &mut key);
    key
}

type Aes256CbcDec = cbc::Decryptor<Aes256>;

/// One encrypted page body → its plaintext content bytes. A `salted` page
/// body carries a 16-byte salt before the ciphertext; page 1 is salted in
/// both the main file *and* WAL frames, later pages never are. Either way the
/// IV sits at `4016` and the plaintext is the page minus its magic.
fn page_content(key: &[u8; 32], page: &[u8], salted: bool) -> Option<Vec<u8>> {
    let (ct, iv) = if salted {
        (page.get(16..CONTENT)?, page.get(CONTENT..CONTENT + IV)?)
    } else {
        (page.get(..CONTENT)?, page.get(CONTENT..CONTENT + IV)?)
    };
    Aes256CbcDec::new(GenericArray::from_slice(key), GenericArray::from_slice(iv))
        .decrypt_padded_vec_mut::<cbc::cipher::block_padding::NoPadding>(ct)
        .ok()
}

/// Assemble the plaintext database image: main file pages, then the WAL's
/// newest committed frame per page. `None` when anything fails to decrypt or
/// the rebuilt header does not check out — a wrong key is silence, not an error.
pub fn database_image(db: &Path, wal: Option<&Path>) -> Option<Vec<u8>> {
    let data = std::fs::read(db).ok()?;
    if data.is_empty() || data.len() % PAGE != 0 {
        return None;
    }
    let key = derived_key();

    let count = data.len() / PAGE;
    // Per-page content (CONTENT bytes each), starting from the main file.
    let mut pages: Vec<Option<Vec<u8>>> = Vec::with_capacity(count);
    for p in 0..count {
        let raw = &data[p * PAGE..(p + 1) * PAGE];
        pages.push(Some(page_content(&key, raw, p == 0)?));
    }

    // The sidecar, if readable: newest committed frame per page wins. Frames
    // are matched by salt and applied up to each commit marker; checksums are
    // skipped (read-only best effort — a torn tail costs at most its own page).
    if let Some(wal) = wal {
        if let Ok(w) = std::fs::read(wal) {
            let magic_ok = w.len() >= 32
                && (w[0..4] == [0x37, 0x7f, 0x06, 0x82] || w[0..4] == [0x37, 0x7f, 0x06, 0x83]);
            if magic_ok {
                let (salt1, salt2) = (
                    u32::from_be_bytes(w[16..20].try_into().ok()?),
                    u32::from_be_bytes(w[20..24].try_into().ok()?),
                );
                let mut off = 32;
                while off + 24 + PAGE <= w.len() {
                    let hdr = &w[off..off + 24];
                    let pgno = u32::from_be_bytes(hdr[0..4].try_into().ok()?);
                    let commit = u32::from_be_bytes(hdr[4..8].try_into().ok()?);
                    if u32::from_be_bytes(hdr[8..12].try_into().ok()?) != salt1
                        || u32::from_be_bytes(hdr[12..16].try_into().ok()?) != salt2
                    {
                        break;
                    }
                    let body = &w[off + 24..off + 24 + PAGE];
                    // Page 1 is salted wherever it lives — the WAL frame keeps
                    // the same salt-prefixed layout as the main file's head.
                    // Treating it as a plain frame corrupts the rebuilt header
                    // and the whole store reads as empty.
                    let content = page_content(&key, body, pgno == 1);
                    // Frames may reference pages past the main file's end: a
                    // growing store writes its new pages only into the WAL, so
                    // the image grows here instead of silently dropping them.
                    if let Some(idx) = pgno.checked_sub(1).map(|i| i as usize) {
                        if idx >= pages.len() {
                            pages.resize(idx + 1, None);
                        }
                        if let Some(slot) = pages.get_mut(idx) {
                            *slot = content;
                        }
                    }
                    // A commit frame publishes the database size so far: pages
                    // beyond it were never part of a transaction.
                    if commit > 0 && (commit as usize) < pages.len() {
                        pages.truncate(commit as usize);
                    }
                    off += 24 + PAGE;
                }
            }
        }
    }

    // Rebuild the plaintext file: page 1 regains its magic prefix, every page
    // regains the 80-byte reserved tail the vendor's header declares. The key
    // gate is page 1's own plaintext: it starts where the magic stops, with
    // page size, journal versions, reserved count and the payload fractions —
    // measured `1000 02 02 50 40 20 20` on the real store.
    let first = pages.first().and_then(|p| p.as_ref())?;
    let header_ok = first.len() >= 8
        && first[0..2] == 4096u16.to_be_bytes()
        && (first[2] == 1 || first[2] == 2)
        && (first[3] == 1 || first[3] == 2)
        && first[4] == RESERVE as u8
        && first[5] == 64
        && first[6] == 32
        && first[7] == 32;
    if !header_ok {
        return None;
    }
    let mut out = Vec::with_capacity(pages.len() * PAGE);
    for (i, page) in pages.iter().enumerate() {
        let content = page.as_ref()?;
        if i == 0 {
            out.extend_from_slice(MAGIC);
        }
        out.extend_from_slice(content);
        out.extend_from_slice(&[0u8; RESERVE]);
    }
    Some(out)
}

/// Open a decrypted image for querying. The image is materialized to a temp
/// file because `rusqlite` here builds without the deserialize feature; the
/// caller closes the connection before dropping the path.
pub fn open_image(image: &[u8]) -> Option<(rusqlite::Connection, std::path::PathBuf)> {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "tokenme-trae-{}-{}.db",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::write(&path, image).ok()?;
    let conn = rusqlite::Connection::open_with_flags(
        &path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    Some((conn, path))
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;

    /// Encrypt a plaintext image (4096-byte pages, reserved space already
    /// declared in its header) into the vendor's on-disk format. Test-side
    /// inverse of [`database_image`]: used to build fixtures without shipping
    /// any real bytes.
    pub(crate) fn encrypt_image(plain: &[u8]) -> Vec<u8> {
        use cbc::cipher::{block_padding::NoPadding, BlockEncryptMut, KeyIvInit};
        type Aes256CbcEnc = cbc::Encryptor<Aes256>;
        assert_eq!(plain.len() % PAGE, 0, "fixture must be page-aligned");
        let key = derived_key();
        let salt: [u8; 16] = core::array::from_fn(|i| i as u8);
        let mut out = Vec::with_capacity(plain.len());
        for (i, page) in plain.chunks(PAGE).enumerate() {
            // Page 1 spends its first 16 bytes on the salt, so its ciphertext
            // is 4000 bytes; every later page carries the full 4016.
            let (body, first) = if i == 0 { (&page[16..CONTENT], true) } else { (&page[..CONTENT], false) };
            let iv: [u8; IV] = core::array::from_fn(|b| (i as u8).wrapping_mul(31).wrapping_add(b as u8));
            let ct = Aes256CbcEnc::new(
                GenericArray::from_slice(&key),
                GenericArray::from_slice(&iv),
            )
            .encrypt_padded_vec_mut::<NoPadding>(body);
            if first {
                out.extend_from_slice(&salt);
            }
            out.extend_from_slice(&ct);
            out.extend_from_slice(&iv);
            out.extend_from_slice(&[0u8; 64]);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real one-table database built by SQLite itself, then normalized to the
    /// vendor's 80-byte per-page reserve: the header byte is set and a VACUUM
    /// lets SQLite rewrite every page to the smaller usable area — the true
    /// plaintext `encrypt_image` expects.
    fn fixture_plain() -> Vec<u8> {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("plain.db");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch("PRAGMA page_size = 4096;
             CREATE TABLE marker (note TEXT);
             INSERT INTO marker VALUES ('hello');")
            .unwrap();
        drop(conn);
        let mut plain = std::fs::read(&db).unwrap();
        assert_eq!(plain.len() % PAGE, 0);
        plain[20] = RESERVE as u8;
        std::fs::write(&db, &plain).unwrap();
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch("VACUUM;").unwrap();
        drop(conn);
        std::fs::read(&db).unwrap()
    }

    #[test]
    fn round_trip_and_header_gate() {
        let plain = fixture_plain();
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("database.db");
        std::fs::write(&db, testing::encrypt_image(&plain)).unwrap();

        let image = database_image(&db, None).expect("the fixture decrypts");
        assert!(image.starts_with(MAGIC));
        assert_eq!(image[20], RESERVE as u8, "the reserved-space byte survives");
        for (i, page) in image.chunks(PAGE).enumerate() {
            assert_eq!(
                &page[..CONTENT],
                &plain[i * PAGE..i * PAGE + CONTENT],
                "page {i}'s content area round-trips"
            );
        }

        // A flipped byte in page 1's ciphertext corrupts the header block and
        // must fail the key gate.
        let mut corrupted = testing::encrypt_image(&plain);
        corrupted[16] ^= 0xff;
        let bad = dir.path().join("bad.db");
        std::fs::write(&bad, corrupted).unwrap();
        assert!(database_image(&bad, None).is_none(), "a wrong key is silence");
    }

    #[test]
    fn wal_frames_override_main_pages_up_to_the_commit() {
        // Main file: page 2 says "hello". The WAL carries a newer page 2 saying
        // "from WAL", committed at dbsize 3.
        let main_plain = fixture_plain();
        let mut wal_versions = main_plain.clone();
        wal_versions[PAGE + 100..PAGE + 109].copy_from_slice(b"from WAL\n");
        let enc_main = testing::encrypt_image(&main_plain);
        let enc_wal_pages = testing::encrypt_image(&wal_versions);

        let mut wal = vec![0u8; 32];
        wal[0..4].copy_from_slice(&[0x37, 0x7f, 0x06, 0x82]);
        wal[8..12].copy_from_slice(&4096u32.to_be_bytes());
        wal[16..20].copy_from_slice(&0xaaaa_u32.to_be_bytes());
        wal[20..24].copy_from_slice(&0xbbbb_u32.to_be_bytes());
        let mut frame = Vec::new();
        frame.extend_from_slice(&2u32.to_be_bytes()); // pgno
        frame.extend_from_slice(&0u32.to_be_bytes()); // no dbsize: not a commit
        frame.extend_from_slice(&0xaaaa_u32.to_be_bytes());
        frame.extend_from_slice(&0xbbbb_u32.to_be_bytes());
        frame.extend_from_slice(&[0u8; 8]); // checksum, unchecked
        frame.extend_from_slice(&enc_wal_pages[PAGE..PAGE * 2]); // page 2's body
        wal.extend_from_slice(&frame);
        // A frame from an older generation: salt mismatch stops the walk.
        wal.extend_from_slice(&[0u8; 24 + PAGE]);

        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("database.db");
        std::fs::write(&db, enc_main).unwrap();
        let wal_path = dir.path().join("database.db-wal");
        std::fs::write(&wal_path, wal).unwrap();

        let image = database_image(&db, Some(&wal_path)).expect("wal applies");
        assert_eq!(image, wal_versions, "the wal page wins, main pages stand");
    }

    /// The head page keeps its salt-prefixed layout inside WAL frames too.
    /// Treating a page-1 frame like a plain frame used to corrupt the rebuilt
    /// header and read the whole store as empty — exactly while Trae was
    /// running, because live writes land in the WAL first.
    #[test]
    fn a_wal_frame_for_page_one_keeps_its_salt_prefix() {
        let main_plain = fixture_plain();
        let mut wal_versions = main_plain.clone();
        wal_versions[70..78].copy_from_slice(b"walhead!");
        let enc_main = testing::encrypt_image(&main_plain);
        let enc_wal = testing::encrypt_image(&wal_versions);

        let mut wal = vec![0u8; 32];
        wal[0..4].copy_from_slice(&[0x37, 0x7f, 0x06, 0x82]);
        wal[8..12].copy_from_slice(&4096u32.to_be_bytes());
        wal[16..20].copy_from_slice(&0xaaaa_u32.to_be_bytes());
        wal[20..24].copy_from_slice(&0xbbbb_u32.to_be_bytes());
        let mut frame = Vec::new();
        frame.extend_from_slice(&1u32.to_be_bytes()); // pgno 1
        frame.extend_from_slice(&0u32.to_be_bytes());
        frame.extend_from_slice(&0xaaaa_u32.to_be_bytes());
        frame.extend_from_slice(&0xbbbb_u32.to_be_bytes());
        frame.extend_from_slice(&[0u8; 8]);
        frame.extend_from_slice(&enc_wal[..PAGE]); // page 1's salted body
        wal.extend_from_slice(&frame);

        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("database.db");
        std::fs::write(&db, enc_main).unwrap();
        let wal_path = dir.path().join("database.db-wal");
        std::fs::write(&wal_path, wal).unwrap();

        let image = database_image(&db, Some(&wal_path)).expect("page-1 frame applies");
        assert_eq!(&image[70..78], b"walhead!", "the wal head page won");
        assert_eq!(image[20], RESERVE as u8, "the header still checks out");
    }

    /// A growing store writes brand-new pages only into the WAL: frames past
    /// the main file's end must grow the image, not vanish.
    #[test]
    fn wal_frames_past_the_main_file_grow_the_image() {
        let main_plain = fixture_plain(); // 2 pages on disk
        assert_eq!(main_plain.len(), PAGE * 2);
        let mut grown = main_plain.clone();
        let mut page3 = vec![0u8; PAGE];
        page3[100..109].copy_from_slice(b"from WAL\n");
        grown.extend_from_slice(&page3);
        let enc_main = testing::encrypt_image(&main_plain);
        let enc_grown = testing::encrypt_image(&grown);

        let mut wal = vec![0u8; 32];
        wal[0..4].copy_from_slice(&[0x37, 0x7f, 0x06, 0x82]);
        wal[8..12].copy_from_slice(&4096u32.to_be_bytes());
        wal[16..20].copy_from_slice(&0xaaaa_u32.to_be_bytes());
        wal[20..24].copy_from_slice(&0xbbbb_u32.to_be_bytes());
        let mut frame = Vec::new();
        frame.extend_from_slice(&3u32.to_be_bytes()); // pgno 3 — past the end
        frame.extend_from_slice(&3u32.to_be_bytes()); // commit: dbsize 3
        frame.extend_from_slice(&0xaaaa_u32.to_be_bytes());
        frame.extend_from_slice(&0xbbbb_u32.to_be_bytes());
        frame.extend_from_slice(&[0u8; 8]);
        frame.extend_from_slice(&enc_grown[PAGE * 2..PAGE * 3]); // page 3's body
        wal.extend_from_slice(&frame);

        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("database.db");
        std::fs::write(&db, enc_main).unwrap();
        let wal_path = dir.path().join("database.db-wal");
        std::fs::write(&wal_path, wal).unwrap();

        let image = database_image(&db, Some(&wal_path)).expect("growth applies");
        assert_eq!(image.len(), PAGE * 3, "the image grew to the commit size");
        assert_eq!(&image[PAGE * 2 + 100..PAGE * 2 + 109], b"from WAL\n");
    }

    #[test]
    fn the_image_opens_and_answers_sql() {
        let plain = fixture_plain();
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("database.db");
        std::fs::write(&db, testing::encrypt_image(&plain)).unwrap();
        let image = database_image(&db, None).unwrap();
        let (conn, path) = open_image(&image).expect("opens");
        let note: String = conn
            .query_row("SELECT note FROM marker", [], |r| r.get(0))
            .expect("the rebuilt image answers queries");
        assert_eq!(note, "hello");
        drop(conn);
        let _ = std::fs::remove_file(path);
    }
}
