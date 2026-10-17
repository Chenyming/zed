use anyhow::{Context as _, Result};
use async_compression::futures::bufread::GzipDecoder;
use collections::HashMap;
use futures::{AsyncRead, AsyncReadExt as _, io::BufReader, lock::Mutex};
use gpui::AsyncApp;
use settings::{RegisterSetting, Settings};
use sha2::{Digest as _, Sha256};
use std::{
    ffi::OsString,
    fs::Metadata,
    path::{Path, PathBuf},
    sync::LazyLock,
    time::SystemTime,
};
use util::shell::ShellKind;

static CONTENT_DIGESTS: LazyLock<ContentDigestCache> = LazyLock::new(ContentDigestCache::default);

#[derive(RegisterSetting)]
struct CopiedServerSettings {
    path: Option<String>,
}

impl Settings for CopiedServerSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        Self {
            path: content.remote.copy_remote_server.clone(),
        }
    }
}

pub(super) async fn configured_path(cx: &mut AsyncApp) -> Result<Option<PathBuf>> {
    let configured = cx
        .update(|cx| CopiedServerSettings::try_get(cx).and_then(|settings| settings.path.clone()));
    resolve_path(
        configured.as_deref(),
        std::env::var_os("ZED_COPY_REMOTE_SERVER"),
    )
    .await
}

async fn resolve_path(
    configured: Option<&str>,
    environment: Option<OsString>,
) -> Result<Option<PathBuf>> {
    let Some(path) = configured
        .map(PathBuf::from)
        .or_else(|| environment.map(PathBuf::from))
    else {
        return Ok(None);
    };
    let path = if let Ok(relative_path) = path.strip_prefix("~") {
        util::paths::home_dir().join(relative_path)
    } else {
        path
    };
    let metadata = smol::fs::metadata(&path)
        .await
        .with_context(|| format!("Reading copy_remote_server file at {}", path.display()))?;
    anyhow::ensure!(
        metadata.is_file(),
        "copy_remote_server must point to a file: {}",
        path.display()
    );
    Ok(Some(path))
}

#[derive(Default)]
struct ContentDigestCache {
    entries: Mutex<HashMap<(PathBuf, bool), CachedDigest>>,
    #[cfg(test)]
    computations: std::sync::atomic::AtomicUsize,
}

struct CachedDigest {
    fingerprint: FileFingerprint,
    digest: String,
}

#[derive(PartialEq, Eq)]
struct FileFingerprint {
    length: u64,
    modified: SystemTime,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    changed: (i64, i64),
}

impl FileFingerprint {
    fn from_metadata(metadata: &Metadata) -> Result<Self> {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt as _;

        anyhow::ensure!(metadata.is_file(), "Copied remote server must be a file");
        Ok(Self {
            length: metadata.len(),
            modified: metadata.modified()?,
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
            #[cfg(unix)]
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        })
    }
}

impl ContentDigestCache {
    async fn get(&self, path: &Path) -> Result<String> {
        let compressed = path.extension().is_some_and(|extension| extension == "gz");
        let path = smol::fs::canonicalize(path).await?;
        let key = (path.clone(), compressed);

        // Keep concurrent connections from hashing the same large archive twice.
        let mut entries = self.entries.lock().await;
        let file = smol::fs::File::open(&path).await?;
        let fingerprint = FileFingerprint::from_metadata(&file.metadata().await?)?;
        if let Some(cached) = entries.get(&key)
            && cached.fingerprint == fingerprint
        {
            return Ok(cached.digest.clone());
        }

        let mut reader: Box<dyn AsyncRead + Unpin + Send> = if compressed {
            let mut decoder = GzipDecoder::new(BufReader::new(file));
            decoder.multiple_members(true);
            Box::new(decoder)
        } else {
            Box::new(file)
        };
        let mut hasher = Sha256::new();
        let mut buffer = [0; 64 * 1024];
        loop {
            let count = reader.read(&mut buffer).await?;
            if count == 0 {
                break;
            }
            hasher.update(&buffer[..count]);
        }
        anyhow::ensure!(
            FileFingerprint::from_metadata(&smol::fs::metadata(&path).await?)? == fingerprint,
            "Copied remote server changed while calculating its SHA-256; retry the connection"
        );
        let digest = format!("{:x}", hasher.finalize());
        entries.insert(
            key,
            CachedDigest {
                fingerprint,
                digest: digest.clone(),
            },
        );
        #[cfg(test)]
        self.computations
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(digest)
    }
}

pub(super) async fn content_digest(path: &Path) -> Result<String> {
    CONTENT_DIGESTS.get(path).await.with_context(|| {
        format!(
            "Calculating copied remote server SHA-256 for {}",
            path.display()
        )
    })
}

pub(super) fn force_upload(value: Option<&str>) -> Result<bool> {
    match value.map(str::trim) {
        None | Some("" | "0" | "false") => Ok(false),
        Some("1" | "true") => Ok(true),
        Some(_) => anyhow::bail!("ZED_COPY_REMOTE_SERVER_FORCE must be 1, true, 0, or false"),
    }
}

pub(super) fn force_upload_from_environment() -> Result<bool> {
    match std::env::var("ZED_COPY_REMOTE_SERVER_FORCE") {
        Ok(value) => force_upload(Some(&value)),
        Err(std::env::VarError::NotPresent) => Ok(false),
        Err(error) => Err(error).context("Reading ZED_COPY_REMOTE_SERVER_FORCE"),
    }
}

// Recording the verified temporary file's identity before either rename makes
// interleaved installs yield a cache miss rather than trust another binary.
const FILE_IDENTITY: &str = "file_identity() { \
    stat -c '%d:%i:%s:%y' \"$1\" 2>/dev/null || stat -f '%d:%i:%z:%m' \"$1\"; \
    };";

fn validate_digest(digest: &str) -> Result<()> {
    anyhow::ensure!(
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "Invalid copied remote server SHA-256"
    );
    Ok(())
}

fn quote_path(path: &str) -> Result<String> {
    anyhow::ensure!(
        !path.contains('\n'),
        "Multiline remote paths are not supported"
    );
    Ok(ShellKind::Posix
        .try_quote(path)
        .context("Quoting copied remote server path")?
        .into_owned())
}

pub(super) fn cache_check_script(destination: &str, digest: &str) -> Result<String> {
    validate_digest(digest)?;
    let record = quote_path(&format!("{destination}.copy-sha256"))?;
    let destination = quote_path(destination)?;
    Ok(format!(
        "set -eu; {FILE_IDENTITY} \
        if [ -x {destination} ] && [ -f {record} ]; then \
            identity=$(file_identity {destination}); \
            expected=$(printf '%s\\n%s' '{digest}' \"$identity\"); \
            recorded=$(cat {record}); \
            if [ \"$recorded\" = \"$expected\" ]; then \
                printf '\\nZED_COPY_CACHE_HIT\\n'; exit 0; \
            fi; \
        fi; \
        printf '\\nZED_COPY_CACHE_MISS\\n'"
    ))
}

pub(super) fn cache_hit(output: &str) -> Result<bool> {
    match output.lines().rev().find(|line| !line.trim().is_empty()) {
        Some("ZED_COPY_CACHE_HIT") => Ok(true),
        Some("ZED_COPY_CACHE_MISS") => Ok(false),
        _ => anyhow::bail!("Invalid copied remote server cache response: {output:?}"),
    }
}

pub(super) fn install_script(destination: &str, temporary: &str, digest: &str) -> Result<String> {
    validate_digest(digest)?;
    let record = quote_path(&format!("{destination}.copy-sha256"))?;
    let temporary_record = quote_path(&format!("{temporary}.copy-sha256"))?;
    let destination = quote_path(destination)?;
    let temporary_binary = temporary.strip_suffix(".gz").unwrap_or(temporary);
    let temporary_binary = quote_path(temporary_binary)?;
    let decompress = if temporary.ends_with(".gz") {
        format!("gunzip -f {}; ", quote_path(temporary)?)
    } else {
        String::new()
    };
    Ok(format!(
        "set -eu; {FILE_IDENTITY} \
        {decompress}if command -v sha256sum >/dev/null 2>&1; then \
            actual=$(sha256sum < {temporary_binary}); \
        else \
            actual=$(shasum -a 256 < {temporary_binary}); \
        fi; \
        actual=${{actual%% *}}; \
        if [ \"$actual\" != '{digest}' ]; then \
            printf '%s\\n' 'Copied remote server SHA-256 mismatch; keeping the installed binary' >&2; \
            exit 1; \
        fi; \
        chmod 755 {temporary_binary}; \
        {temporary_binary} version >/dev/null; \
        identity=$(file_identity {temporary_binary}); \
        printf '%s\\n%s\\n' '{digest}' \"$identity\" > {temporary_record}; \
        mv -f {temporary_binary} {destination}; \
        mv -f {temporary_record} {record}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_compression::futures::write::GzipEncoder;
    use futures::{AsyncWriteExt as _, io::Cursor};
    use std::sync::atomic::Ordering;

    fn temporary_directory() -> Result<tempfile::TempDir> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tmp");
        std::fs::create_dir_all(&root)?;
        Ok(tempfile::Builder::new()
            .prefix("copied-server-test-")
            .tempdir_in(root)?)
    }

    async fn gzip(contents: &[u8]) -> Result<Vec<u8>> {
        let mut encoder = GzipEncoder::new(Cursor::new(Vec::new()));
        encoder.write_all(contents).await?;
        encoder.close().await?;
        Ok(encoder.into_inner().into_inner())
    }

    #[gpui::test]
    fn copy_remote_server_setting_updates(cx: &mut gpui::App) {
        let mut store = settings::SettingsStore::new(cx, &settings::default_settings());
        assert!(store.get::<CopiedServerSettings>(None).path.is_none());
        for (json, expected) in [
            (
                r#"{"copy_remote_server":"~/servers/remote_server"}"#,
                Some("~/servers/remote_server"),
            ),
            (
                r#"{"copy_remote_server":"~/servers/remote_server.gz"}"#,
                Some("~/servers/remote_server.gz"),
            ),
            (r#"{"copy_remote_server":null}"#, None),
        ] {
            let result = store.set_user_settings(json, cx);
            assert_eq!(result.parse_status, settings::ParseStatus::Success);
            assert_eq!(
                store.get::<CopiedServerSettings>(None).path.as_deref(),
                expected
            );
        }
    }

    #[test]
    fn configured_path_precedes_environment_without_fallback() -> Result<()> {
        smol::block_on(async {
            let directory = temporary_directory()?;
            let binary = directory.path().join("server");
            let missing = directory.path().join("missing");
            std::fs::write(&binary, b"server")?;
            let binary_string = binary.to_str().context("binary path")?;
            let missing_string = missing.to_str().context("missing path")?;

            assert_eq!(resolve_path(None, None).await?, None);
            assert_eq!(
                resolve_path(None, Some(binary.clone().into())).await?,
                Some(binary.clone())
            );
            assert_eq!(
                resolve_path(Some(binary_string), Some(missing.clone().into())).await?,
                Some(binary.clone())
            );
            let error = resolve_path(Some(missing_string), Some(binary.into()))
                .await
                .expect_err("Missing configured file must stop the connection");
            assert!(error.to_string().contains("copy_remote_server"));
            assert!(error.to_string().contains(missing_string));
            Ok(())
        })
    }

    #[test]
    fn invalid_paths_do_not_fall_back_to_environment() -> Result<()> {
        smol::block_on(async {
            let directory = temporary_directory()?;
            let binary = directory.path().join("server");
            std::fs::write(&binary, b"server")?;
            for invalid in ["", directory.path().to_str().context("directory path")?] {
                assert!(
                    resolve_path(Some(invalid), Some(binary.clone().into()))
                        .await
                        .is_err()
                );
            }
            assert!(
                resolve_path(None, Some(directory.path().join("missing").into()))
                    .await
                    .is_err()
            );
            Ok(())
        })
    }

    #[test]
    fn raw_and_gzip_hash_the_same_binary() -> Result<()> {
        smol::block_on(async {
            let directory = temporary_directory()?;
            let raw = directory.path().join("server");
            let archive = directory.path().join("server.gz");
            let alternate_archive = directory.path().join("alternate.gz");
            let payload = b"same server binary\0\xff";
            std::fs::write(&raw, payload)?;
            let mut compressed = gzip(payload).await?;
            std::fs::write(&archive, &compressed)?;
            compressed
                .get_mut(4..8)
                .context("gzip timestamp")?
                .copy_from_slice(&123456_u32.to_le_bytes());
            std::fs::write(&alternate_archive, compressed)?;
            let cache = ContentDigestCache::default();
            let expected = format!("{:x}", Sha256::digest(payload));
            assert_eq!(cache.get(&raw).await?, expected);
            assert_eq!(cache.get(&archive).await?, expected);
            assert_eq!(cache.get(&alternate_archive).await?, expected);
            Ok(())
        })
    }

    #[test]
    fn concatenated_gzip_members_hash_all_contents() -> Result<()> {
        smol::block_on(async {
            let directory = temporary_directory()?;
            let archive = directory.path().join("server.gz");
            let mut compressed = gzip(b"first").await?;
            compressed.extend(gzip(b"second").await?);
            std::fs::write(&archive, compressed)?;
            assert_eq!(
                ContentDigestCache::default().get(&archive).await?,
                format!("{:x}", Sha256::digest(b"firstsecond"))
            );
            Ok(())
        })
    }

    #[test]
    fn repeated_and_concurrent_connections_hash_once_per_lifetime() -> Result<()> {
        smol::block_on(async {
            let directory = temporary_directory()?;
            let archive = directory.path().join("server.gz");
            std::fs::write(&archive, gzip(b"cached binary").await?)?;
            let cache = ContentDigestCache::default();
            let (first, concurrent) = futures::try_join!(cache.get(&archive), cache.get(&archive))?;
            assert_eq!(first, concurrent);
            assert_eq!(cache.get(&archive).await?, first);
            assert_eq!(cache.computations.load(Ordering::Relaxed), 1);
            let next_lifetime = ContentDigestCache::default();
            assert_eq!(next_lifetime.get(&archive).await?, first);
            assert_eq!(next_lifetime.computations.load(Ordering::Relaxed), 1);
            Ok(())
        })
    }

    #[test]
    fn changed_or_replaced_files_invalidate_the_process_cache() -> Result<()> {
        smol::block_on(async {
            let directory = temporary_directory()?;
            let archive = directory.path().join("server.gz");
            let cache = ContentDigestCache::default();
            std::fs::write(&archive, gzip(b"old").await?)?;
            let old = cache.get(&archive).await?;
            std::fs::write(&archive, gzip(b"larger new binary").await?)?;
            let changed = cache.get(&archive).await?;
            assert_ne!(changed, old);
            let replacement = directory.path().join("replacement.gz");
            std::fs::write(&replacement, gzip(b"replaced contents").await?)?;
            std::fs::rename(replacement, &archive)?;
            assert_ne!(cache.get(&archive).await?, changed);
            assert_eq!(cache.computations.load(Ordering::Relaxed), 3);
            Ok(())
        })
    }

    #[test]
    fn failed_gzip_reads_are_not_cached() -> Result<()> {
        smol::block_on(async {
            let directory = temporary_directory()?;
            let archive = directory.path().join("server.gz");
            let cache = ContentDigestCache::default();
            std::fs::write(&archive, b"not gzip")?;
            assert!(cache.get(&archive).await.is_err());
            assert_eq!(cache.computations.load(Ordering::Relaxed), 0);
            std::fs::write(&archive, gzip(b"valid server").await?)?;
            assert_eq!(
                cache.get(&archive).await?,
                format!("{:x}", Sha256::digest(b"valid server"))
            );
            assert_eq!(cache.computations.load(Ordering::Relaxed), 1);
            Ok(())
        })
    }

    #[test]
    fn force_upload_and_cache_responses_are_validated() -> Result<()> {
        for value in [None, Some(""), Some("0"), Some("false")] {
            assert!(!force_upload(value)?);
        }
        for value in [Some("1"), Some("true"), Some(" true ")] {
            assert!(force_upload(value)?);
        }
        assert!(force_upload(Some("yes")).is_err());
        assert!(cache_hit("login banner\nZED_COPY_CACHE_HIT\r\n")?);
        assert!(!cache_hit("ZED_COPY_CACHE_MISS\n")?);
        assert!(cache_hit("unexpected reply").is_err());
        assert!(cache_check_script("server", "invalid digest").is_err());
        assert!(install_script("server", "upload", "invalid digest").is_err());
        Ok(())
    }

    #[cfg(unix)]
    fn execute_script(script: &str) -> Result<std::process::Output> {
        assert!(
            !script.contains('\n'),
            "SSH command arguments must be single-line"
        );
        Ok(std::process::Command::new("sh")
            .args(["-c", script])
            .output()?)
    }

    #[cfg(unix)]
    fn installed_cache_hit(destination: &Path, digest: &str) -> Result<bool> {
        let output = execute_script(&cache_check_script(
            destination.to_str().context("destination path")?,
            digest,
        )?)?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        cache_hit(std::str::from_utf8(&output.stdout)?)
    }

    #[cfg(unix)]
    const SERVER: &[u8] = b"#!/bin/sh\n[ \"$1\" = version ] || exit 2\nprintf '1.25.0\\n'\n";

    #[cfg(unix)]
    #[test]
    fn installed_binary_and_record_allow_reuse_and_detect_replacement() -> Result<()> {
        smol::block_on(async {
            let directory = temporary_directory()?;
            let destination = directory.path().join("server '$;[with spaces]");
            let temporary = directory.path().join("upload '$;[with spaces].gz");
            let digest = format!("{:x}", Sha256::digest(SERVER));
            assert!(!installed_cache_hit(&destination, &digest)?);
            std::fs::write(&temporary, gzip(SERVER).await?)?;
            let output = execute_script(&install_script(
                destination.to_str().context("destination path")?,
                temporary.to_str().context("temporary path")?,
                &digest,
            )?)?;
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(std::fs::read(&destination)?, SERVER);
            assert!(installed_cache_hit(&destination, &digest)?);
            assert!(!installed_cache_hit(&destination, &"0".repeat(64))?);
            let replacement = directory.path().join("replacement");
            std::fs::copy(&destination, &replacement)?;
            std::fs::rename(replacement, &destination)?;
            assert!(!installed_cache_hit(&destination, &digest)?);
            std::fs::remove_file(&destination)?;
            assert!(!installed_cache_hit(&destination, &digest)?);
            Ok(())
        })
    }

    #[cfg(unix)]
    #[test]
    fn corrupt_upload_does_not_replace_installed_binary_or_record() -> Result<()> {
        let directory = temporary_directory()?;
        let destination = directory.path().join("server");
        let temporary = directory.path().join("upload");
        let destination_str = destination.to_str().context("destination path")?;
        let temporary_str = temporary.to_str().context("temporary path")?;
        let digest = format!("{:x}", Sha256::digest(SERVER));
        std::fs::write(&temporary, SERVER)?;
        let output = execute_script(&install_script(destination_str, temporary_str, &digest)?)?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let record_path = format!("{destination_str}.copy-sha256");
        let record = std::fs::read(&record_path)?;
        std::fs::write(&temporary, b"corrupt upload")?;
        let output = execute_script(&install_script(destination_str, temporary_str, &digest)?)?;
        assert!(!output.status.success());
        assert_eq!(std::fs::read(&destination)?, SERVER);
        assert_eq!(std::fs::read(&record_path)?, record);
        assert!(installed_cache_hit(&destination, &digest)?);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn non_executable_server_does_not_replace_installed_binary() -> Result<()> {
        let directory = temporary_directory()?;
        let destination = directory.path().join("server");
        let temporary = directory.path().join("upload");
        std::fs::write(&destination, SERVER)?;
        let invalid = b"#!/bin/sh\nexit 13\n";
        std::fs::write(&temporary, invalid)?;
        let digest = format!("{:x}", Sha256::digest(invalid));
        let output = execute_script(&install_script(
            destination.to_str().context("destination path")?,
            temporary.to_str().context("temporary path")?,
            &digest,
        )?)?;
        assert!(!output.status.success());
        assert_eq!(std::fs::read(&destination)?, SERVER);
        assert!(!Path::new(&format!("{}.copy-sha256", destination.display())).exists());
        Ok(())
    }
}
