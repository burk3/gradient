/*
 * SPDX-FileCopyrightText: 2026 Wavelens GmbH <info@wavelens.io>
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use futures::StreamExt as _;
use sha2::{Digest, Sha256};
use std::io::{Cursor, Read};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnpackedTarball {
    pub nar_hash: String,
    pub newest_mtime: i64,
}

pub async fn nar_hash_of_dir(path: &Path) -> Result<String> {
    let mut stream = harmonia_file_nar::NarByteStream::new(path.to_path_buf());
    let mut hasher = Sha256::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("reading NAR byte stream")?;
        hasher.update(&chunk);
    }

    let digest = hasher.finalize();

    Ok(format!(
        "sha256-{}",
        base64::engine::general_purpose::STANDARD.encode(digest)
    ))
}

pub async fn tarball_source_nar_hash(req: reqwest::RequestBuilder) -> Result<UnpackedTarball> {
    let resp = req.send().await.context("fetching source tarball")?;
    tarball_response_nar_hash(resp).await
}

pub async fn tarball_response_nar_hash(resp: reqwest::Response) -> Result<UnpackedTarball> {
    let bytes = resp
        .error_for_status()
        .context("source tarball request failed")?
        .bytes()
        .await
        .context("reading source tarball body")?
        .to_vec();

    tarball_bytes_nar_hash(bytes).await
}

pub async fn tarball_bytes_nar_hash(bytes: Vec<u8>) -> Result<UnpackedTarball> {
    let tmp = tempfile::tempdir().context("creating extraction temp dir")?;
    let dest = tmp.path().to_path_buf();
    let newest_mtime = tokio::task::spawn_blocking(move || unpack_tarball(&bytes, &dest))
        .await
        .context("extraction task panicked")??;
    let root = source_root(tmp.path())?;

    Ok(UnpackedTarball {
        nar_hash: nar_hash_of_dir(&root).await?,
        newest_mtime,
    })
}

fn decompressor(bytes: &[u8]) -> Result<Box<dyn Read + '_>> {
    const GZIP: &[u8] = &[0x1f, 0x8b];
    const XZ: &[u8] = &[0xfd, b'7', b'z', b'X', b'Z', 0x00];
    const ZSTD: &[u8] = &[0x28, 0xb5, 0x2f, 0xfd];

    let cursor = Cursor::new(bytes);
    Ok(if bytes.starts_with(GZIP) {
        Box::new(flate2::read::GzDecoder::new(cursor))
    } else if bytes.starts_with(XZ) {
        Box::new(xz2::read::XzDecoder::new(cursor))
    } else if bytes.starts_with(ZSTD) {
        Box::new(zstd::stream::read::Decoder::new(cursor).context("starting zstd decoder")?)
    } else {
        Box::new(cursor)
    })
}

fn unpack_tarball(bytes: &[u8], dest: &Path) -> Result<i64> {
    let mut archive = tar::Archive::new(decompressor(bytes)?);
    archive.set_preserve_permissions(true);
    archive.set_preserve_mtime(false);

    let mut newest = 0i64;
    for entry in archive.entries().context("reading tar entries")? {
        let mut entry = entry.context("reading tar entry")?;
        if matches!(
            entry.header().entry_type(),
            tar::EntryType::XGlobalHeader | tar::EntryType::XHeader
        ) {
            continue;
        }

        let mtime = entry.header().mtime().context("reading entry mtime")?;
        newest = newest.max(i64::try_from(mtime).unwrap_or(i64::MAX));

        let path = entry.path().context("entry path")?.display().to_string();
        let is_dir = entry.header().entry_type() == tar::EntryType::Directory;
        if !entry.unpack_in(dest).with_context(|| format!("unpacking {path}"))? {
            bail!("tar entry `{path}` escapes the extraction directory");
        }

        // Directory modes don't affect the NAR hash; a read-only one would block unpacking its children.
        if is_dir {
            let dir = dest.join(&path);
            let mut perms = std::fs::metadata(&dir)
                .with_context(|| format!("reading permissions of {path}"))?
                .permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&dir, perms)
                .with_context(|| format!("making {path} writable"))?;
        }
    }

    Ok(newest)
}

/// Nix hashes the sole top-level directory when there is exactly one, else the archive root.
fn source_root(dir: &Path) -> Result<PathBuf> {
    let mut entries = std::fs::read_dir(dir).context("listing extracted tarball")?;
    let first = entries.next().transpose().context("reading extracted entry")?;
    if let (Some(only), None) = (first, entries.next())
        && only.file_type().context("extracted entry type")?.is_dir()
    {
        return Ok(only.path());
    }

    Ok(dir.to_path_buf())
}

#[cfg(test)]
pub(crate) mod test_tarball {
    pub fn tar_of(entries: &[(&str, &[u8], u64)]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (path, data, mtime) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Regular);
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_mtime(*mtime);
            builder.append_data(&mut header, path, *data).unwrap();
        }
        builder.into_inner().unwrap()
    }

    pub fn targz(entries: &[(&str, &[u8], u64)]) -> Vec<u8> {
        use std::io::Write as _;
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(&tar_of(entries)).unwrap();
        enc.finish().unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::test_tarball::{tar_of, targz};
    use super::*;
    use std::io::Write as _;

    const SOURCE: &[(&str, &[u8], u64)] = &[
        ("source/flake.nix", b"{ outputs = _: { }; }", 100),
        ("source/lib/a.nix", b"1", 300),
        ("source/lib/b.nix", b"2", 200),
    ];

    async fn expected_hash(files: &[(&str, &[u8])]) -> String {
        let dir = tempfile::tempdir().unwrap();
        for (path, data) in files {
            let out = dir.path().join(path);
            std::fs::create_dir_all(out.parent().unwrap()).unwrap();
            std::fs::write(out, data).unwrap();
        }
        nar_hash_of_dir(dir.path()).await.unwrap()
    }

    fn xz(bytes: &[u8]) -> Vec<u8> {
        let mut enc = xz2::write::XzEncoder::new(Vec::new(), 1);
        enc.write_all(bytes).unwrap();
        enc.finish().unwrap()
    }

    #[tokio::test]
    async fn every_compression_hashes_the_stripped_source_dir() {
        let expected = expected_hash(&[
            ("flake.nix", b"{ outputs = _: { }; }"),
            ("lib/a.nix", b"1"),
            ("lib/b.nix", b"2"),
        ])
        .await;
        let tar = tar_of(SOURCE);
        let zst = zstd::stream::encode_all(tar.as_slice(), 1).unwrap();

        for (name, bytes) in [
            ("gz", targz(SOURCE)),
            ("xz", xz(&tar)),
            ("zst", zst),
            ("plain", tar.clone()),
        ] {
            let got = tarball_bytes_nar_hash(bytes).await.unwrap();
            assert_eq!(got.nar_hash, expected, "{name} tarball hashed differently");
        }
    }

    #[tokio::test]
    async fn newest_mtime_is_the_max_over_entries() {
        let got = tarball_bytes_nar_hash(targz(SOURCE)).await.unwrap();
        assert_eq!(got.newest_mtime, 300);
    }

    #[tokio::test]
    async fn several_top_level_entries_are_hashed_unstripped() {
        let entries: &[(&str, &[u8], u64)] = &[("a", b"1", 1), ("b", b"2", 1)];
        let got = tarball_bytes_nar_hash(targz(entries)).await.unwrap();
        assert_eq!(got.nar_hash, expected_hash(&[("a", b"1"), ("b", b"2")]).await);
    }

    #[tokio::test]
    async fn pax_global_header_is_ignored() {
        let mut builder = tar::Builder::new(Vec::new());
        let record = b"18 comment=abcdef\n";
        let mut pax = tar::Header::new_ustar();
        pax.set_entry_type(tar::EntryType::XGlobalHeader);
        pax.set_size(record.len() as u64);
        builder.append_data(&mut pax, "pax_global_header", &record[..]).unwrap();
        let mut file = tar::Header::new_gnu();
        file.set_entry_type(tar::EntryType::Regular);
        file.set_size(1);
        file.set_mode(0o644);
        builder.append_data(&mut file, "repo-abc/f", &b"x"[..]).unwrap();
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(&builder.into_inner().unwrap()).unwrap();

        let got = tarball_bytes_nar_hash(enc.finish().unwrap()).await.unwrap();
        assert_eq!(got.nar_hash, expected_hash(&[("f", b"x")]).await);
    }

    #[tokio::test]
    async fn path_traversal_entries_are_rejected() {
        let mut header = tar::Header::new_gnu();
        let name = b"../evil";
        header.as_old_mut().name[..name.len()].copy_from_slice(name);
        header.set_entry_type(tar::EntryType::Regular);
        header.set_size(1);
        header.set_mode(0o644);
        header.set_cksum();
        let mut builder = tar::Builder::new(Vec::new());
        builder.append(&header, &b"x"[..]).unwrap();
        let tar = builder.into_inner().unwrap();

        let err = tarball_bytes_nar_hash(tar).await.unwrap_err();
        assert!(format!("{err:#}").contains("escapes"), "unexpected error: {err:#}");
    }

    #[tokio::test]
    async fn read_only_directories_do_not_block_extraction() {
        let mut builder = tar::Builder::new(Vec::new());
        let mut dir = tar::Header::new_gnu();
        dir.set_entry_type(tar::EntryType::Directory);
        dir.set_size(0);
        dir.set_mode(0o555);
        builder.append_data(&mut dir, "source/ro/", &[][..]).unwrap();
        let mut file = tar::Header::new_gnu();
        file.set_entry_type(tar::EntryType::Regular);
        file.set_size(1);
        file.set_mode(0o644);
        builder.append_data(&mut file, "source/ro/f", &b"x"[..]).unwrap();
        let tar = builder.into_inner().unwrap();

        let got = tarball_bytes_nar_hash(tar).await.unwrap();
        assert_eq!(got.nar_hash, expected_hash(&[("ro/f", b"x")]).await);
    }

    #[tokio::test]
    #[ignore = "network: compares against hashes written by nix into ~/src/all-the-nix/flake.lock"]
    async fn matches_nix_for_nixos_channel_xz_and_zst() {
        let client = gradient_util::http::download_client();
        for (url, nar_hash) in [
            (
                "https://releases.nixos.org/nixos/unstable/nixos-26.05pre924538.3497aa5c9457/nixexprs.tar.xz",
                "sha256-8bW3q88CEg2u4hSP66Vf4lpbLonHz7hqDNBMcCY7E9U=",
            ),
            (
                "https://releases.nixos.org/nixos/unstable/nixos-26.11pre1072397.eaad089433ca/nixexprs.tar.zst",
                "sha256-eQUZVehWLDBbpBR2rq6DnuRee1ULT6Y675RhsYDSjYI=",
            ),
        ] {
            let got = tarball_source_nar_hash(client.get(url)).await.unwrap();
            assert_eq!(got.nar_hash, nar_hash, "{url}");
        }
    }
}
