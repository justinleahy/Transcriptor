//! Whisper model catalogue and downloads (ggml files from Hugging Face).

use crate::models::{events, ModelInfo, ModelProgress};
use anyhow::{anyhow, Context, Result};
use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tauri::{AppHandle, Emitter};
use tokio::io::AsyncWriteExt;

/// Hugging Face repository the models come from.
const HF_REPO: &str = "ggerganov/whisper.cpp";
/// Immutable commit of `HF_REPO` the catalog digests were taken from. Changing
/// this requires refreshing every `sha256` / `size_bytes` below from the LFS
/// pointers at the new revision.
pub const HF_REVISION: &str = "5359861c739e955e79d9a303bcbc70fb988958b1";

/// whisper.cpp model files start with GGML_FILE_MAGIC written as a
/// little-endian u32 (bytes "lmgg").
const GGML_MAGIC: u32 = 0x6767_6d6c;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// No bytes for this long aborts the download.
const READ_TIMEOUT: Duration = Duration::from_secs(60);
/// Hard ceiling for a whole download.
const TOTAL_TIMEOUT: Duration = Duration::from_secs(30 * 60);

pub struct CatalogEntry {
    pub name: &'static str,
    pub size_mb: u64,
    /// Exact file size in bytes at `HF_REVISION`.
    pub size_bytes: u64,
    /// Lowercase hex SHA-256 of the file at `HF_REVISION`.
    pub sha256: &'static str,
}

macro_rules! entry {
    ($name:literal, $mb:literal, $bytes:literal, $sha:literal) => {
        CatalogEntry { name: $name, size_mb: $mb, size_bytes: $bytes, sha256: $sha }
    };
}

pub const CATALOG: &[CatalogEntry] = &[
    entry!("tiny.en", 75, 77_704_715, "921e4cf8686fdd993dcd081a5da5b6c365bfde1162e72b08d75ac75289920b1f"),
    entry!("tiny", 75, 77_691_713, "be07e048e1e599ad46341c8d2a135645097a538221678b7acdd1b1919c6e1b21"),
    entry!("base.en", 142, 147_964_211, "a03779c86df3323075f5e796cb2ce5029f00ec8869eee3fdfb897afe36c6d002"),
    entry!("base", 142, 147_951_465, "60ed5bc3dd14eea856493d334349b405782ddcaf0028d4b5df4088345fba2efe"),
    entry!("small.en", 466, 487_614_201, "c6138d6d58ecc8322097e0f987c32f1be8bb0a18532a3f88f734d1bbf9c41e5d"),
    entry!("small", 466, 487_601_967, "1be3a9b2063867b937e64e2ec7483364a79917e157fa98c5d94b5c1fffea987b"),
    entry!("medium.en", 1500, 1_533_774_781, "cc37e93478338ec7700281a7ac30a10128929eb8f427dda2e865faa8f6da4356"),
    entry!("medium", 1500, 1_533_763_059, "6c14d5adee5f86394037b4e4e8b59f1673b6cee10e3cf0b11bbdbee79c156208"),
    entry!("large-v3-turbo", 1600, 1_624_555_275, "1fc70f774d38eb169993ac391eea357ef47c88757ef72ee5943879b7e8e2bc69"),
];

fn download_url(name: &str) -> String {
    format!("https://huggingface.co/{HF_REPO}/resolve/{HF_REVISION}/ggml-{name}.bin")
}

/// Streaming integrity check: exact size, SHA-256, and ggml magic.
struct Verifier {
    expected_size: u64,
    expected_sha256: &'static str,
    hasher: Sha256,
    received: u64,
    head: Vec<u8>,
}

impl Verifier {
    fn new(entry: &CatalogEntry) -> Self {
        Verifier {
            expected_size: entry.size_bytes,
            expected_sha256: entry.sha256,
            hasher: Sha256::new(),
            received: 0,
            head: Vec::with_capacity(4),
        }
    }

    /// Feed the next bytes; fails as soon as the file grows past the expected size.
    fn update(&mut self, bytes: &[u8]) -> Result<()> {
        self.received += bytes.len() as u64;
        if self.received > self.expected_size {
            return Err(anyhow!(
                "download is larger than expected ({} bytes); aborting",
                self.expected_size
            ));
        }
        if self.head.len() < 4 {
            let take = (4 - self.head.len()).min(bytes.len());
            self.head.extend_from_slice(&bytes[..take]);
        }
        self.hasher.update(bytes);
        Ok(())
    }

    fn finish(self) -> Result<()> {
        if self.received != self.expected_size {
            return Err(anyhow!(
                "download is incomplete ({} of {} bytes)",
                self.received,
                self.expected_size
            ));
        }
        let digest = format!("{:x}", self.hasher.finalize());
        if digest != self.expected_sha256 {
            return Err(anyhow!(
                "model checksum mismatch (expected sha256 {}, got {digest}); the file was discarded",
                self.expected_sha256
            ));
        }
        let magic = <[u8; 4]>::try_from(self.head.as_slice()).map(u32::from_le_bytes).ok();
        if magic != Some(GGML_MAGIC) {
            return Err(anyhow!("downloaded file is not a ggml model (bad magic)"));
        }
        Ok(())
    }
}

pub fn models_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("models")
}

pub fn model_path(data_dir: &Path, name: &str) -> PathBuf {
    models_dir(data_dir).join(format!("ggml-{name}.bin"))
}

fn catalog_entry(name: &str) -> Result<&'static CatalogEntry> {
    CATALOG
        .iter()
        .find(|e| e.name == name)
        .ok_or_else(|| anyhow!("unknown model '{name}'"))
}

pub fn list(data_dir: &Path) -> Vec<ModelInfo> {
    CATALOG
        .iter()
        .map(|e| {
            let p = model_path(data_dir, e.name);
            let downloaded = p.is_file();
            ModelInfo {
                name: e.name.to_string(),
                size_mb: e.size_mb,
                downloaded,
                path: downloaded.then(|| p.to_string_lossy().to_string()),
            }
        })
        .collect()
}

pub fn info(data_dir: &Path, name: &str) -> Result<ModelInfo> {
    let e = catalog_entry(name)?;
    let p = model_path(data_dir, name);
    let downloaded = p.is_file();
    Ok(ModelInfo {
        name: e.name.to_string(),
        size_mb: e.size_mb,
        downloaded,
        path: downloaded.then(|| p.to_string_lossy().to_string()),
    })
}

pub fn delete(data_dir: &Path, name: &str) -> Result<()> {
    catalog_entry(name)?;
    let p = model_path(data_dir, name);
    if p.exists() {
        std::fs::remove_file(&p).with_context(|| format!("removing {}", p.display()))?;
    }
    Ok(())
}

pub async fn download(app: &AppHandle, data_dir: &Path, name: &str) -> Result<ModelInfo> {
    let entry = catalog_entry(name)?;
    let dir = models_dir(data_dir);
    tokio::fs::create_dir_all(&dir).await?;
    crate::fs_perms::harden_dir(&dir);
    let final_path = model_path(data_dir, name);
    if final_path.is_file() {
        return info(data_dir, name);
    }
    // Unique temp name so concurrent downloads never share a file.
    let part_path = dir.join(format!("ggml-{name}.bin.{}.part", uuid::Uuid::new_v4()));
    if let Err(e) = download_verified(app, entry, &part_path, &final_path).await {
        let _ = tokio::fs::remove_file(&part_path).await;
        return Err(e);
    }
    let _ = app.emit(
        events::MODEL_PROGRESS,
        ModelProgress {
            name: name.to_string(),
            downloaded: entry.size_bytes,
            total: entry.size_bytes,
        },
    );
    info(data_dir, name)
}

/// Download into `part_path`, verify it, then fsync and atomically rename to
/// `final_path`. The caller removes `part_path` on error.
async fn download_verified(
    app: &AppHandle,
    entry: &CatalogEntry,
    part_path: &Path,
    final_path: &Path,
) -> Result<()> {
    let name = entry.name;
    let url = download_url(name);
    let client = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(READ_TIMEOUT)
        .timeout(TOTAL_TIMEOUT)
        .build()?;
    let resp = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("requesting {url}"))?
        .error_for_status()
        .with_context(|| format!("downloading {url}"))?;
    if let Some(len) = resp.content_length() {
        if len != entry.size_bytes {
            return Err(anyhow!(
                "server reported {len} bytes for {name}, expected {}",
                entry.size_bytes
            ));
        }
    }
    let total = entry.size_bytes;

    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(part_path)
        .await
        .with_context(|| format!("creating {}", part_path.display()))?;
    crate::fs_perms::harden_file(part_path);
    let mut verifier = Verifier::new(entry);
    let mut stream = resp.bytes_stream();
    let mut last_emit = std::time::Instant::now();
    while let Some(item) = stream.next().await {
        let bytes = item.context("reading download stream")?;
        verifier.update(&bytes)?;
        file.write_all(&bytes).await?;
        if last_emit.elapsed().as_millis() > 150 {
            last_emit = std::time::Instant::now();
            let _ = app.emit(
                events::MODEL_PROGRESS,
                ModelProgress {
                    name: name.to_string(),
                    downloaded: verifier.received,
                    total,
                },
            );
        }
    }
    file.flush().await?;
    verifier.finish().with_context(|| format!("verifying {name}"))?;
    file.sync_all().await?;
    drop(file);
    tokio::fs::rename(part_path, final_path).await?;
    crate::fs_perms::harden_file(final_path);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_entry(data: &[u8]) -> CatalogEntry {
        let sha: &'static str = Box::leak(format!("{:x}", Sha256::digest(data)).into_boxed_str());
        CatalogEntry { name: "test", size_mb: 0, size_bytes: data.len() as u64, sha256: sha }
    }

    fn ggml_bytes() -> Vec<u8> {
        let mut v = GGML_MAGIC.to_le_bytes().to_vec();
        v.extend_from_slice(&[7u8; 100]);
        v
    }

    #[test]
    fn catalog_is_pinned_and_well_formed() {
        assert_eq!(HF_REVISION.len(), 40);
        assert!(download_url("tiny.en").contains(HF_REVISION));
        assert!(!download_url("tiny.en").contains("/main/"));
        for e in CATALOG {
            assert_eq!(e.sha256.len(), 64, "{}", e.name);
            assert!(e.sha256.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
            assert!(e.size_bytes > 1_000_000);
        }
        assert_eq!(&GGML_MAGIC.to_le_bytes(), b"lmgg");
    }

    #[test]
    fn accepts_matching_file_split_across_chunks() {
        let data = ggml_bytes();
        let entry = test_entry(&data);
        let mut v = Verifier::new(&entry);
        v.update(&data[..1]).unwrap();
        v.update(&data[1..3]).unwrap();
        v.update(&data[3..]).unwrap();
        v.finish().unwrap();
    }

    #[test]
    fn rejects_oversize_early() {
        let data = ggml_bytes();
        let entry = test_entry(&data);
        let mut v = Verifier::new(&entry);
        v.update(&data).unwrap();
        assert!(v.update(b"x").is_err());
    }

    #[test]
    fn rejects_truncated_corrupt_and_non_ggml() {
        let data = ggml_bytes();
        let entry = test_entry(&data);

        let mut v = Verifier::new(&entry);
        v.update(&data[..50]).unwrap();
        assert!(v.finish().unwrap_err().to_string().contains("incomplete"));

        let mut bad = data.clone();
        bad[60] ^= 1;
        let mut v = Verifier::new(&entry);
        v.update(&bad).unwrap();
        assert!(v.finish().unwrap_err().to_string().contains("checksum"));

        let mut not_ggml = data.clone();
        not_ggml[0] = b'X';
        let entry = test_entry(&not_ggml);
        let mut v = Verifier::new(&entry);
        v.update(&not_ggml).unwrap();
        assert!(v.finish().unwrap_err().to_string().contains("magic"));
    }
}
