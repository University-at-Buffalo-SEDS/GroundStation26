//! Shared bounded raster validation for probes, new downloads and cheap resume checks.
use anyhow::{Result, bail};
use image::{ImageFormat, ImageReader};
use std::{
    io::{Cursor, Read, Seek, SeekFrom},
    path::Path,
    sync::{Arc, OnceLock},
};

pub(crate) fn validate(bytes: &[u8]) -> Result<(&'static str, u32, u32)> {
    if bytes.len() > crate::network::MAX_TILE_BYTES {
        bail!("image exceeds 512 KiB")
    }
    let format = image::guess_format(bytes)?;
    let name = match format {
        ImageFormat::Png => "PNG",
        ImageFormat::Jpeg => "JPEG",
        _ => bail!("tile is not PNG or JPEG"),
    };
    if !cached_header_ok(
        &bytes[..bytes.len().min(32)],
        &bytes[bytes.len().saturating_sub(12)..],
        bytes.len() as u64,
    ) {
        bail!("tile has invalid dimensions or a missing/truncated PNG/JPEG header or trailer");
    }
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(4096);
    limits.max_image_height = Some(4096);
    limits.max_alloc = Some(64 * 1024 * 1024);
    reader.limits(limits);
    let image = reader.decode()?;
    Ok((name, image.width(), image.height()))
}

pub(crate) async fn validate_download(bytes: Vec<u8>) -> Result<Vec<u8>> {
    static DECODERS: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();
    let permit = DECODERS
        .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(4)))
        .clone()
        .acquire_owned()
        .await?;
    // Keep HTTP concurrency independent of CPU decoding, with no body copy and
    // at most four bounded decoded images alive. Cancellation retains the
    // permit until the blocking decoder actually finishes.
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        validate(&bytes)?;
        Ok(bytes)
    })
    .await?
}

pub(crate) fn cached_header_ok(head: &[u8], tail: &[u8], length: u64) -> bool {
    if length > crate::network::MAX_TILE_BYTES as u64 {
        return false;
    }
    if head.starts_with(b"\xff\xd8\xff") {
        return length >= 4 && tail.ends_with(b"\xff\xd9");
    }
    const IEND: &[u8] = b"\0\0\0\0IEND\xae\x42\x60\x82";
    if length < 45
        || head.len() < 24
        || !head.starts_with(b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR")
        || !tail.ends_with(IEND)
    {
        return false;
    }
    let width = u32::from_be_bytes(head[16..20].try_into().unwrap());
    let height = u32::from_be_bytes(head[20..24].try_into().unwrap());
    (1..=4096).contains(&width) && (1..=4096).contains(&height)
}
pub(crate) fn cached_file_ok(path: &Path) -> Result<bool> {
    let mut file = std::fs::File::open(path)?;
    let length = file.metadata()?.len();
    let mut head = [0; 32];
    let n = length.min(32) as usize;
    file.read_exact(&mut head[..n])?;
    let mut tail = [0; 12];
    let count = length.min(12) as usize;
    file.seek(SeekFrom::End(-(count as i64)))?;
    file.read_exact(&mut tail[..count])?;
    Ok(cached_header_ok(&head[..n], &tail[..count], length))
}
#[cfg(test)]
pub(crate) fn image() -> Vec<u8> {
    let mut bytes = Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(2, 2)
        .write_to(&mut bytes, ImageFormat::Png)
        .unwrap();
    bytes.into_inner()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cheap_resume_check_rejects_html_and_truncation_but_accepts_real_rasters() {
        for format in [ImageFormat::Png, ImageFormat::Jpeg] {
            let mut image = Cursor::new(Vec::new());
            image::DynamicImage::new_rgb8(2, 2)
                .write_to(&mut image, format)
                .unwrap();
            let bytes = image.into_inner();
            assert!(validate(&bytes).is_ok());
            assert!(cached_header_ok(
                &bytes[..bytes.len().min(32)],
                &bytes[bytes.len().saturating_sub(12)..],
                bytes.len() as u64
            ));
            let truncated = &bytes[..bytes.len() / 2];
            assert!(!cached_header_ok(
                &truncated[..truncated.len().min(32)],
                &truncated[truncated.len().saturating_sub(12)..],
                truncated.len() as u64
            ));
        }
        assert!(!cached_header_ok(b"<html>Unavailable</html>", b"html>", 24));
    }
    #[tokio::test]
    async fn download_validation_never_accepts_successful_html_or_truncated_images() {
        for bytes in [
            b"<html>try later</html>".to_vec(),
            b"\xff\xd8\xfftile-data".to_vec(),
            b"\x89PNG\r\n\x1a\n".to_vec(),
        ] {
            assert!(validate_download(bytes).await.is_err());
        }
        for format in [ImageFormat::Png, ImageFormat::Jpeg] {
            let mut body = Cursor::new(Vec::new());
            image::DynamicImage::new_rgb8(2, 2)
                .write_to(&mut body, format)
                .unwrap();
            let mut body = body.into_inner();
            body.pop();
            assert!(
                validate_download(body).await.is_err(),
                "decoder must not silently accept missing image trailers"
            );
        }
        let valid = image();
        assert_eq!(validate_download(valid.clone()).await.unwrap(), valid);
    }
}
