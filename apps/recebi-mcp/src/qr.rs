use std::{
    fmt::Write as FmtWrite,
    fs::{self, OpenOptions},
    io::{Cursor, Write as IoWrite},
    path::{Path, PathBuf},
};

use getrandom::fill;
use image::{ColorType, ImageEncoder, Luma, codecs::png::PngEncoder};
use qrcode::{EcLevel, QrCode};
use recebi_core::limits::MAX_SOLANA_PAY_URL_BYTES;
use sha2::{Digest, Sha256};
use thiserror::Error;

const QR_DIRECTORY: &str = "qr";
const QR_MIN_DIMENSION: u32 = 512;
const MAX_QR_PNG_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum QrError {
    #[error("QR payload is too large")]
    PayloadTooLarge,
    #[error("QR encoding failed")]
    Encoding,
    #[error("QR image storage is unavailable")]
    StorageUnavailable,
    #[error("QR image is too large")]
    ImageTooLarge,
    #[error("secure random generation is unavailable")]
    RandomUnavailable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QrArtifact {
    pub path: PathBuf,
    pub attachment_marker: String,
    pub png_sha256: String,
}

/// Render the trusted Solana Pay URL into a Telegram-compatible PNG.
///
/// The output is written below the trusted Recebi data directory with private
/// permissions and atomically published. The caller supplies the already
/// persisted canonical URL; this function never constructs payment terms.
pub fn render_to_file(
    data_dir: &Path,
    receivable_id: &str,
    solana_pay_url: &str,
) -> Result<QrArtifact, QrError> {
    if solana_pay_url.is_empty() || solana_pay_url.len() > MAX_SOLANA_PAY_URL_BYTES {
        return Err(QrError::PayloadTooLarge);
    }

    let code = QrCode::with_error_correction_level(solana_pay_url.as_bytes(), EcLevel::M)
        .map_err(|_| QrError::Encoding)?;
    let image = code
        .render::<Luma<u8>>()
        .min_dimensions(QR_MIN_DIMENSION, QR_MIN_DIMENSION)
        .build();
    let mut png = Vec::new();
    PngEncoder::new(Cursor::new(&mut png))
        .write_image(
            image.as_raw(),
            image.width(),
            image.height(),
            ColorType::L8.into(),
        )
        .map_err(|_| QrError::Encoding)?;
    if png.len() > MAX_QR_PNG_BYTES {
        return Err(QrError::ImageTooLarge);
    }

    let qr_directory = data_dir.join(QR_DIRECTORY);
    fs::create_dir_all(&qr_directory).map_err(|_| QrError::StorageUnavailable)?;
    set_private_directory(&qr_directory)?;

    let id_digest = digest_hex(receivable_id.as_bytes());
    let file_name = format!("qr-{id_digest}.png");
    let target = qr_directory.join(&file_name);
    let mut random = [0_u8; 8];
    fill(&mut random).map_err(|_| QrError::RandomUnavailable)?;
    let temporary = qr_directory.join(format!(".{file_name}.{}.tmp", digest_hex(&random)));

    let write_result = write_private_atomic(&temporary, &target, &png);
    if write_result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    write_result?;

    let path = fs::canonicalize(&target).map_err(|_| QrError::StorageUnavailable)?;
    let path_text = path.to_string_lossy().into_owned();
    Ok(QrArtifact {
        path,
        attachment_marker: format!("[IMAGE:{path_text}]"),
        png_sha256: digest_hex(&png),
    })
}

fn write_private_atomic(temporary: &Path, target: &Path, bytes: &[u8]) -> Result<(), QrError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    set_private_file_options(&mut options);
    let mut file = options
        .open(temporary)
        .map_err(|_| QrError::StorageUnavailable)?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|_| QrError::StorageUnavailable)?;
    set_private_file(temporary)?;
    fs::rename(temporary, target).map_err(|_| QrError::StorageUnavailable)
}

fn digest_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        FmtWrite::write_fmt(&mut output, format_args!("{byte:02x}")).expect("string write");
    }
    output
}

#[cfg(unix)]
fn set_private_directory(path: &Path) -> Result<(), QrError> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|_| QrError::StorageUnavailable)
}

#[cfg(not(unix))]
fn set_private_directory(_path: &Path) -> Result<(), QrError> {
    Ok(())
}

#[cfg(unix)]
fn set_private_file_options(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;

    options.mode(0o600);
}

#[cfg(not(unix))]
fn set_private_file_options(_options: &mut OpenOptions) {}

#[cfg(unix)]
fn set_private_file(path: &Path) -> Result<(), QrError> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|_| QrError::StorageUnavailable)
}

#[cfg(not(unix))]
fn set_private_file(_path: &Path) -> Result<(), QrError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use image::GenericImageView;
    use tempfile::tempdir;

    use super::render_to_file;

    #[test]
    fn renders_a_private_png_for_the_exact_payload() {
        let directory = tempdir().expect("directory");
        let artifact = render_to_file(
            directory.path(),
            "WATCH-001",
            "solana:11111111111111111111111111111111?amount=0.01&%73%70%6C%2D%74%6F%6B%65%6E=11111111111111111111111111111111&reference=US517G5965aydkZ46HS38QLi7UQiSojurfbQfKCELFx&label=Recebi%20412",
        )
        .expect("QR");
        assert!(artifact.path.is_absolute());
        assert_eq!(
            artifact.attachment_marker,
            format!("[IMAGE:{}]", artifact.path.display())
        );
        assert_eq!(
            artifact.path.extension().and_then(|ext| ext.to_str()),
            Some("png")
        );
        let bytes = fs::read(&artifact.path).expect("PNG");
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
        let image = image::load_from_memory(&bytes).expect("valid PNG");
        assert!(image.dimensions().0 >= 512);
        assert!(image.dimensions().1 >= 512);
        assert!(!artifact.png_sha256.is_empty());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            assert_eq!(
                fs::metadata(directory.path().join("qr"))
                    .expect("qr dir")
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(&artifact.path)
                    .expect("qr file")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn rerendering_same_id_replaces_the_same_artifact_path() {
        let directory = tempdir().expect("directory");
        let first = render_to_file(directory.path(), "WATCH-001", "solana:first").expect("first");
        let second =
            render_to_file(directory.path(), "WATCH-001", "solana:second").expect("second");
        assert_eq!(first.path, second.path);
        assert_ne!(first.png_sha256, second.png_sha256);
    }
}
