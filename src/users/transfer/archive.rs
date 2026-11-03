//! Archive container indexing and validation (epic 31 section 7):
//! format detection by magic bytes, one bounded pass over members,
//! and rejection of hostile member sets before any data is applied.

use std::collections::{HashMap, HashSet};
use std::io::{Cursor, Read};

use crate::{Error, Result};

/// Safety bounds applied to every archive regardless of container.
const MAX_MEMBERS: usize = 10_000;
const MAX_MEMBER_BYTES: usize = 64 * 1024 * 1024;
const MAX_TOTAL_BYTES: usize = 512 * 1024 * 1024;
/// Compressed→uncompressed sanity bound (deflate bombs).
const MAX_RATIO: u64 = 100;

/// A fully indexed archive: member name → contents. `*.pem`/key
/// material is dropped at indexing time — it is never stored,
/// logged, or re-exported.
pub struct ArchiveIndex {
    members: HashMap<String, Vec<u8>>,
}

impl ArchiveIndex {
    /// Fetch a member's bytes by normalized name.
    pub fn get(&self, name: &str) -> Option<&[u8]> {
        self.members.get(name).map(Vec::as_slice)
    }

    /// Whether a member exists.
    pub fn contains(&self, name: &str) -> bool {
        self.members.contains_key(name)
    }

    /// Normalize a single wrapper directory: some archives nest every
    /// member under one top-level folder, which bare-name lookups
    /// (`outbox.json`) would otherwise miss. Stripped only when *all*
    /// members share that one prefix — an index with root members or
    /// mixed roots is returned unchanged.
    pub fn strip_wrapper(self) -> ArchiveIndex {
        let prefix = self
            .members
            .keys()
            .next()
            .and_then(|first| first.split_once('/').map(|(p, _)| p.to_string()));
        let Some(prefix) = prefix else { return self };
        if !self
            .members
            .keys()
            .all(|name| name.starts_with(&format!("{prefix}/")))
        {
            return self;
        }
        let members = self
            .members
            .into_iter()
            .map(|(name, data)| (name[prefix.len() + 1..].to_string(), data))
            .collect();
        ArchiveIndex { members }
    }
}

/// Detect the container from magic bytes (never the filename) and
/// index its members, applying the shared safety bounds.
pub fn index(bytes: &[u8]) -> Result<ArchiveIndex> {
    if bytes.starts_with(b"PK\x03\x04") || bytes.starts_with(b"PK\x05\x06") {
        index_zip(bytes)
    } else if bytes.starts_with(b"\x1f\x8b") {
        index_targz(bytes)
    } else {
        Err(Error::Validation("archive is not a zip or tar.gz".into()))
    }
}

/// A member name is a plain archive-relative path: no traversal, no
/// absolute prefix, no drive letters, no empty segments. Leading
/// `./` segments (the `tar -C dir .` layout real archives ship) are
/// normalized away before validation.
fn normalize_name(name: &str) -> Option<String> {
    let name = name.trim_start_matches("./").to_string();
    if name.is_empty() || name.starts_with('/') || name.starts_with('\\') {
        return None;
    }
    if name.contains(':') {
        return None;
    }
    let bad = name
        .split(['/', '\\'])
        .any(|seg| seg.is_empty() || seg == ".." || seg == ".");
    if bad {
        return None;
    }
    Some(name)
}

/// Skip key material entirely — pre-4.2 Mastodon archives shipped
/// `key.pem`/`actor.pem`; the bytes must never be indexed. The name
/// check is case-insensitive; `is_key_content` catches renamed keys.
fn is_key_member(name: &str) -> bool {
    name.rsplit('/')
        .next()
        .unwrap_or(name)
        .to_ascii_lowercase()
        .ends_with(".pem")
}

/// PEM/armored key material under any member name is dropped after
/// the bounded read — never indexed, stored, or re-exported. Leading
/// whitespace, a BOM, or a short preamble (OpenSSL's `Bag Attributes`
/// style) cannot hide it; JSON members are exempt because
/// `actor.json` legitimately embeds a *public* `publicKeyPem` armor.
fn is_key_content(bytes: &[u8]) -> bool {
    let body = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    let Some(i) = body.iter().position(|b| !b.is_ascii_whitespace()) else {
        return false;
    };
    if matches!(body[i], b'{' | b'[') {
        return false;
    }
    body.windows(10).any(|w| w == b"-----BEGIN")
}

fn check_insert(
    members: &mut HashMap<String, Vec<u8>>,
    seen: &mut HashSet<String>,
    name: &str,
    data: Vec<u8>,
) -> Result<()> {
    if members.len() >= MAX_MEMBERS {
        return Err(Error::Validation("archive has too many members".into()));
    }
    if data.len() > MAX_MEMBER_BYTES {
        return Err(Error::Validation(format!(
            "archive member {name} exceeds the size limit"
        )));
    }
    if !seen.insert(name.to_string()) {
        return Err(Error::Validation(format!(
            "archive has duplicate member {name}"
        )));
    }
    members.insert(name.to_string(), data);
    Ok(())
}

fn index_zip(bytes: &[u8]) -> Result<ArchiveIndex> {
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|e| Error::Validation(format!("archive is not a valid zip: {e}")))?;
    if zip.len() > MAX_MEMBERS {
        return Err(Error::Validation("archive has too many members".into()));
    }
    let mut members = HashMap::new();
    let mut seen = HashSet::new();
    let mut total = 0usize;
    for i in 0..zip.len() {
        let f = zip
            .by_index(i)
            .map_err(|e| Error::Validation(format!("zip member {i}: {e}")))?;
        // Unix-mode entries that are not regular files (symlinks,
        // devices, FIFOs) are hostile surface like their tar cousins;
        // directories are skipped like any container listing.
        match f.unix_mode().map(|m| m & 0o170000) {
            Some(0o040000) => continue,
            Some(0 | 0o100000) | None => {}
            Some(_) => {
                return Err(Error::Validation(
                    "archive contains a non-file member".into(),
                ));
            }
        }
        if !f.is_file() {
            continue;
        }
        let raw_name = f.name().to_string();
        let Some(name) = normalize_name(&raw_name) else {
            return Err(Error::Validation(format!(
                "archive member {raw_name} has an unsafe path"
            )));
        };
        if is_key_member(&name) {
            continue;
        }
        let declared = f.size() as usize;
        if declared > MAX_MEMBER_BYTES || total + declared > MAX_TOTAL_BYTES {
            return Err(Error::Validation("archive exceeds the size limit".into()));
        }
        let compressed = f.compressed_size();
        if compressed > 0 && declared as u64 > compressed.saturating_mul(MAX_RATIO) {
            return Err(Error::Validation(format!(
                "archive member {name} exceeds the compression-ratio limit"
            )));
        }
        // Read bounded: the declared size is attacker-controlled, so
        // the cap applies to actual decompressed bytes, not the header.
        let mut buf = Vec::with_capacity(declared.min(1 << 20));
        f.take(MAX_MEMBER_BYTES as u64 + 1)
            .read_to_end(&mut buf)
            .map_err(|e| Error::Validation(format!("zip member {name}: {e}")))?;
        if buf.len() > MAX_MEMBER_BYTES {
            return Err(Error::Validation(format!(
                "archive member {name} exceeds the size limit"
            )));
        }
        if is_key_content(&buf) {
            continue;
        }
        total += buf.len();
        if total > MAX_TOTAL_BYTES {
            return Err(Error::Validation("archive exceeds the size limit".into()));
        }
        check_insert(&mut members, &mut seen, &name, buf)?;
    }
    Ok(ArchiveIndex { members })
}

fn index_targz(bytes: &[u8]) -> Result<ArchiveIndex> {
    let decoder = flate2::read::GzDecoder::new(bytes);
    let mut tar = tar::Archive::new(decoder);
    let mut members = HashMap::new();
    let mut seen = HashSet::new();
    let mut total = 0usize;
    let mut entry_count = 0usize;
    let entries = tar
        .entries()
        .map_err(|e| Error::Validation(format!("archive is not a valid tar.gz: {e}")))?;
    for entry in entries {
        let entry = entry.map_err(|e| Error::Validation(format!("tar entry: {e}")))?;
        entry_count += 1;
        if entry_count > MAX_MEMBERS {
            return Err(Error::Validation("archive has too many members".into()));
        }
        let kind = entry.header().entry_type();
        match kind {
            // Metadata/directory records carry no member bytes — the
            // reader already consumed GNU/pax records, but skipping
            // them keeps the intent explicit and tolerates a pax
            // global header if one is ever surfaced.
            tar::EntryType::Directory
            | tar::EntryType::GNULongName
            | tar::EntryType::GNULongLink
            | tar::EntryType::XHeader
            | tar::EntryType::XGlobalHeader => continue,
            // Only regular files are importable — symlinks/hardlinks,
            // devices, FIFOs and sparse entries are hostile surface.
            tar::EntryType::Regular => {}
            _ => {
                return Err(Error::Validation(
                    "archive contains a non-file member".into(),
                ));
            }
        }
        let raw_name = entry
            .path()
            .map_err(|e| Error::Validation(format!("tar entry path: {e}")))?
            .to_string_lossy()
            .into_owned();
        let Some(name) = normalize_name(&raw_name) else {
            return Err(Error::Validation(format!(
                "archive member {raw_name} has an unsafe path"
            )));
        };
        // A skipped member's bytes still flow through the decoder
        // (the tar reader discards them to reach the next header), so
        // the declared size bounds skipped members too — a gzip bomb
        // inside `key.pem` is rejected, not streamed to oblivion.
        let declared = entry
            .header()
            .size()
            .map_err(|e| Error::Validation(format!("tar entry header: {e}")))?;
        if declared > MAX_MEMBER_BYTES as u64 {
            return Err(Error::Validation(format!(
                "archive member {name} exceeds the size limit"
            )));
        }
        if is_key_member(&name) {
            total += declared as usize;
            if total > MAX_TOTAL_BYTES {
                return Err(Error::Validation("archive exceeds the size limit".into()));
            }
            continue;
        }
        let mut buf = Vec::new();
        entry
            .take(MAX_MEMBER_BYTES as u64 + 1)
            .read_to_end(&mut buf)
            .map_err(|e| Error::Validation(format!("tar member {name}: {e}")))?;
        if buf.len() > MAX_MEMBER_BYTES {
            return Err(Error::Validation(format!(
                "archive member {name} exceeds the size limit"
            )));
        }
        if is_key_content(&buf) {
            continue;
        }
        total += buf.len();
        if total > MAX_TOTAL_BYTES {
            return Err(Error::Validation("archive exceeds the size limit".into()));
        }
        check_insert(&mut members, &mut seen, &name, buf)?;
    }
    Ok(ArchiveIndex { members })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn zip_of(members: &[(&str, &[u8], Option<u32>)]) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, data, mode) in members {
            let mut o = zip::write::SimpleFileOptions::default();
            if let Some(m) = mode {
                o = o.unix_permissions(*m);
            }
            w.start_file(*name, o).unwrap();
            w.write_all(data).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    #[test]
    fn pem_members_are_never_indexed() {
        let z = zip_of(&[
            ("outbox.json", b"{}".as_slice(), None),
            ("key.pem", b"PRIVATE KEY".as_slice(), None),
        ]);
        let idx = index(&z).unwrap();
        assert!(idx.get("key.pem").is_none());
        assert!(idx.get("outbox.json").is_some());
    }

    #[test]
    fn key_member_check_is_case_insensitive() {
        let z = zip_of(&[
            ("outbox.json", b"{}".as_slice(), None),
            ("KEY.PEM", b"PRIVATE KEY".as_slice(), None),
        ]);
        let idx = index(&z).unwrap();
        assert!(idx.get("KEY.PEM").is_none());
    }

    /// Key material under a non-`.pem` name is dropped after the
    /// bounded read — the content check catches renamed keys.
    #[test]
    fn pem_content_is_never_indexed() {
        let z = zip_of(&[
            ("outbox.json", b"{}".as_slice(), None),
            (
                "notes.txt",
                b"-----BEGIN PRIVATE KEY-----\nabc".as_slice(),
                None,
            ),
        ]);
        let idx = index(&z).unwrap();
        assert!(idx.get("notes.txt").is_none());
        assert!(idx.get("outbox.json").is_some());
    }

    /// Preamble/whitespace before the armor cannot smuggle a key in,
    /// and JSON members keep embedded *public* key armor (actor.json).
    #[test]
    fn pem_content_check_handles_preamble_and_json() {
        let actor =
            serde_json::json!({"publicKey": {"publicKeyPem": "-----BEGIN PUBLIC KEY-----\nx"}});
        let z = zip_of(&[
            ("actor.json", actor.to_string().as_bytes(), None),
            (
                "notes.txt",
                b"\n-----BEGIN PRIVATE KEY-----\nx".as_slice(),
                None,
            ),
            (
                "bag.pem.bin",
                b"Bag Attributes\n    localKeyID: 00\n-----BEGIN PRIVATE KEY-----\nx".as_slice(),
                None,
            ),
            ("outbox.json", b"{}".as_slice(), None),
        ]);
        let idx = index(&z).unwrap();
        assert!(idx.get("actor.json").is_some(), "JSON keeps publicKeyPem");
        assert!(idx.get("notes.txt").is_none(), "leading-ws PEM dropped");
        assert!(idx.get("bag.pem.bin").is_none(), "preamble PEM dropped");
        assert!(idx.get("outbox.json").is_some());
    }

    /// A long run of whitespace cannot push armor past the scan, and a
    /// BOM in front of JSON still exempts the member.
    #[test]
    fn pem_content_check_scans_the_whole_member() {
        let mut long = vec![b' '; 4096];
        long.extend(b"-----BEGIN PRIVATE KEY-----\nx");
        let mut json = b"\xEF\xBB\xBF".to_vec();
        json.extend(br#"{"publicKeyPem":"-----BEGIN PUBLIC KEY-----"}"#);
        let z = zip_of(&[
            ("notes.txt", long.as_slice(), None),
            ("actor.json", json.as_slice(), None),
        ]);
        let idx = index(&z).unwrap();
        assert!(idx.get("notes.txt").is_none(), "padded PEM dropped");
        assert!(
            idx.get("actor.json").is_some(),
            "BOM JSON keeps publicKeyPem"
        );
    }

    /// Minimal CRC32 for hand-built zip fixtures.
    fn crc32(data: &[u8]) -> u32 {
        let mut crc = !0u32;
        for &b in data {
            crc ^= b as u32;
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0xEDB8_8320 & 0u32.wrapping_sub(crc & 1));
            }
        }
        !crc
    }

    /// A stored-method zip with one member carrying raw unix
    /// file-type bits in its external attributes — the only way to
    /// express a symlink member (the writer API masks them off).
    fn raw_zip_symlink() -> Vec<u8> {
        let name = b"evil-link";
        let data = b"/etc/passwd";
        let mut z = Vec::new();
        z.extend(b"PK\x03\x04");
        z.extend(20u16.to_le_bytes());
        z.extend(0u16.to_le_bytes());
        z.extend(0u16.to_le_bytes());
        z.extend(0u16.to_le_bytes());
        z.extend(0u16.to_le_bytes());
        z.extend(crc32(data).to_le_bytes());
        z.extend((data.len() as u32).to_le_bytes());
        z.extend((data.len() as u32).to_le_bytes());
        z.extend((name.len() as u16).to_le_bytes());
        z.extend(0u16.to_le_bytes());
        z.extend(name);
        z.extend(data);
        let cd_off = z.len() as u32;
        z.extend(b"PK\x01\x02");
        z.extend(0x0314u16.to_le_bytes()); // made-by: unix + zip 2.0
        z.extend(20u16.to_le_bytes());
        z.extend(0u16.to_le_bytes());
        z.extend(0u16.to_le_bytes());
        z.extend(0u16.to_le_bytes());
        z.extend(0u16.to_le_bytes());
        z.extend(crc32(data).to_le_bytes());
        z.extend((data.len() as u32).to_le_bytes());
        z.extend((data.len() as u32).to_le_bytes());
        z.extend((name.len() as u16).to_le_bytes());
        z.extend(0u16.to_le_bytes());
        z.extend(0u16.to_le_bytes());
        z.extend(0u16.to_le_bytes());
        z.extend(0u16.to_le_bytes());
        z.extend((0o120777u32 << 16).to_le_bytes()); // symlink mode
        z.extend(0u32.to_le_bytes());
        z.extend(name);
        let cd_size = z.len() as u32 - cd_off;
        z.extend(b"PK\x05\x06");
        z.extend(0u16.to_le_bytes());
        z.extend(0u16.to_le_bytes());
        z.extend(1u16.to_le_bytes());
        z.extend(1u16.to_le_bytes());
        z.extend(cd_size.to_le_bytes());
        z.extend(cd_off.to_le_bytes());
        z.extend(0u16.to_le_bytes());
        z
    }

    #[test]
    fn zip_symlink_is_rejected() {
        assert!(index(&raw_zip_symlink()).is_err());
    }

    #[test]
    fn traversal_is_rejected() {
        let z = zip_of(&[("../evil.json", b"{}".as_slice(), None)]);
        assert!(index(&z).is_err());
        let z = zip_of(&[("a/../b.json", b"{}".as_slice(), None)]);
        assert!(index(&z).is_err());
    }

    #[test]
    fn dot_prefix_is_normalized() {
        let z = zip_of(&[("./outbox.json", b"{}".as_slice(), None)]);
        let idx = index(&z).unwrap();
        assert!(idx.get("outbox.json").is_some());
    }

    #[test]
    fn single_wrapper_directory_is_normalized() {
        let z = zip_of(&[
            ("export/outbox.json", b"{}".as_slice(), None),
            ("export/media_attachments/a.png", b"x".as_slice(), None),
        ]);
        let idx = index(&z).unwrap().strip_wrapper();
        assert!(idx.get("outbox.json").is_some());
        assert!(idx.get("media_attachments/a.png").is_some());
    }

    #[test]
    fn mixed_roots_keep_their_names() {
        let z = zip_of(&[
            ("outbox.json", b"{}".as_slice(), None),
            ("export/extra.txt", b"x".as_slice(), None),
        ]);
        let idx = index(&z).unwrap().strip_wrapper();
        assert!(idx.get("outbox.json").is_some());
        assert!(idx.get("export/extra.txt").is_some());
    }

    #[test]
    fn oversized_member_is_rejected() {
        let big = vec![0u8; MAX_MEMBER_BYTES + 1];
        let z = zip_of(&[("big.bin", big.as_slice(), None)]);
        assert!(index(&z).is_err());
    }

    /// Minimal tar.gz fixture builder.
    fn targz_of(members: &[(&str, &[u8])]) -> Vec<u8> {
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        {
            let mut b = tar::Builder::new(&mut enc);
            for (name, data) in members {
                let mut h = tar::Header::new_gnu();
                h.set_entry_type(tar::EntryType::Regular);
                h.set_size(data.len() as u64);
                h.set_mode(0o644);
                h.set_cksum();
                b.append_data(&mut h, name, *data).unwrap();
            }
            b.finish().unwrap();
        }
        enc.finish().unwrap()
    }

    /// A bomb hidden inside a skipped member (`key.pem`) still flows
    /// through the decoder — the declared-size bound must reject it.
    #[test]
    fn oversized_skipped_member_is_rejected() {
        let zeros = vec![0u8; MAX_MEMBER_BYTES + 1];
        let archive = targz_of(&[("key.pem", zeros.as_slice())]);
        assert!(index(&archive).is_err());
    }

    #[test]
    fn duplicate_names_are_rejected() {
        let z = zip_of(&[
            ("./outbox.json", b"{}".as_slice(), None),
            ("outbox.json", b"{}".as_slice(), None),
        ]);
        assert!(index(&z).is_err());
    }
}
