//! Small read-only connection qualification; never opens a map database or tile path.
use super::{
    BASE_COVERAGE_MAX_ZOOM, BUFFALO_ROCHESTER_BOUNDS, NA_BOUNDS, TEXAS_DESERT_BOUNDS,
    lonlat_to_tile,
};
use crate::network::{MAX_TILE_BYTES, Route, TileClient};
use anyhow::{Result, bail};
use image::{ImageFormat, ImageReader};
use std::io::Cursor;
use std::time::Duration;

fn sample_tiles(max_zoom: u32) -> Vec<(u32, u32, u32)> {
    let mut tiles = vec![(0, 0, 0)];
    let mut add = |bounds: (f64, f64, f64, f64), z| {
        let (x, y) = lonlat_to_tile((bounds.0 + bounds.2) / 2.0, (bounds.1 + bounds.3) / 2.0, z);
        let tile = (z, x, y);
        if !tiles.contains(&tile) {
            tiles.push(tile)
        }
    };
    add(NA_BOUNDS, max_zoom.min(BASE_COVERAGE_MAX_ZOOM));
    if max_zoom > BASE_COVERAGE_MAX_ZOOM {
        add(BUFFALO_ROCHESTER_BOUNDS, max_zoom);
        add(TEXAS_DESERT_BOUNDS, max_zoom);
    }
    tiles
}
fn validate_image(bytes: &[u8]) -> Result<(&'static str, u32, u32)> {
    if bytes.len() > MAX_TILE_BYTES {
        bail!("image exceeds512KiB")
    }
    let format = image::guess_format(bytes)?;
    let name = match format {
        ImageFormat::Png => "PNG",
        ImageFormat::Jpeg => "JPEG",
        _ => bail!("tile is not PNG or JPEG"),
    };
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(4096);
    limits.max_image_height = Some(4096);
    limits.max_alloc = Some(64 * 1024 * 1024);
    reader.limits(limits);
    let image = reader.decode()?;
    Ok((name, image.width(), image.height()))
}
#[derive(Default)]
struct Report {
    passed: usize,
    total: usize,
    direct: usize,
    normal: usize,
    fallback: usize,
    direct_failed: usize,
}
impl Report {
    fn succeeds(&self, direct_requested: bool) -> bool {
        self.passed == self.total
            && (!direct_requested
                || (self.direct == self.total && self.fallback == 0 && self.direct_failed == 0))
    }
}
async fn check(client: &TileClient, tiles: &[(u32, u32, u32)], base_url: &str) -> Report {
    let mut report = Report {
        total: tiles.len(),
        ..Report::default()
    };
    for &(z, x, y) in tiles {
        let url = format!("{base_url}/{z}/{y}/{x}");
        match tokio::time::timeout(Duration::from_secs(35), client.fetch(&url)).await {
            Ok(Ok(response)) => {
                match response.route {
                    Route::Direct => report.direct += 1,
                    Route::Normal => report.normal += 1,
                    Route::NormalFallback => report.fallback += 1,
                };
                report.direct_failed += usize::from(response.direct_failed);
                let prefix = format!(
                    "z={z} x={x} y={y} route={}{}",
                    response.route.label(),
                    if response.direct_failed {
                        " (direct failed)"
                    } else {
                        ""
                    }
                );
                if !response.status.is_success() {
                    println!("{prefix}: FAILED HTTP {}", response.status.as_u16());
                    continue;
                }
                match validate_image(&response.bytes) {
                    Ok((format, w, h)) => {
                        report.passed += 1;
                        println!(
                            "{prefix}: OK {format} {w}x{h}, {} bytes",
                            response.bytes.len()
                        );
                    }
                    Err(_) => {
                        println!("{prefix}: FAILED invalid, truncated or oversized PNG/JPEG image")
                    }
                }
            }
            Ok(Err(_)) => println!("z={z} x={x} y={y}: FAILED request/body; no usable response"),
            Err(_) => println!("z={z} x={x} y={y}: FAILED connection test timed out"),
        }
    }
    report
}
pub(crate) async fn run(
    client: &TileClient,
    region: &str,
    max_zoom: u32,
    base_url: &str,
    direct_requested: bool,
) -> Result<()> {
    let tiles = sample_tiles(max_zoom);
    println!(
        "Map connection test: region={region}, zoom=0..={max_zoom}, {} tiles; no map data will be written.",
        tiles.len()
    );
    let report = check(client, &tiles, base_url).await;
    println!(
        "Map test: {}/{} valid images; direct={}, normal={}, normal-fallback={}, failed direct attempts={}.",
        report.passed,
        report.total,
        report.direct,
        report.normal,
        report.fallback,
        report.direct_failed
    );
    if direct_requested
        && (report.direct != report.total || report.fallback != 0 || report.direct_failed != 0)
    {
        eprintln!(
            "Requested direct interface did not pass every sample; normal downloads can still use fallback."
        );
    }
    if !report.succeeds(direct_requested) {
        bail!("map connection test failed; see route and image results above")
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    fn encoded(format: ImageFormat, width: u32) -> Vec<u8> {
        let image = image::DynamicImage::new_rgb8(width, 1);
        let mut bytes = Cursor::new(Vec::new());
        image.write_to(&mut bytes, format).unwrap();
        bytes.into_inner()
    }
    #[test]
    fn samples_are_small_distinct_and_follow_configured_zoom_bounds() {
        for z in [0, 1, 8, 9, 17] {
            let tiles = sample_tiles(z);
            assert!((1..=4).contains(&tiles.len()));
            let mut unique = tiles.clone();
            unique.sort();
            unique.dedup();
            assert_eq!(unique.len(), tiles.len());
            assert!(
                tiles
                    .iter()
                    .all(|&(zoom, x, y)| zoom <= z && x < 1 << zoom && y < 1 << zoom)
            );
        }
        assert_eq!(sample_tiles(0), vec![(0, 0, 0)]);
        assert_eq!(sample_tiles(17).len(), 4);
    }
    #[test]
    fn real_png_and_jpeg_decode_and_report_dimensions() {
        for format in [ImageFormat::Png, ImageFormat::Jpeg] {
            assert_eq!(validate_image(&encoded(format, 2)).unwrap().1, 2);
        }
    }
    #[test]
    fn html_signature_only_truncated_and_oversized_dimensions_fail() {
        for bytes in [
            b"<html>error</html>".to_vec(),
            b"\x89PNG\r\n\x1a\n".to_vec(),
            encoded(ImageFormat::Png, 8192),
        ] {
            assert!(validate_image(&bytes).is_err());
        }
        let mut jpeg = encoded(ImageFormat::Jpeg, 2);
        jpeg.truncate(jpeg.len() / 2);
        assert!(validate_image(&jpeg).is_err());
    }
    #[test]
    fn explicit_direct_cannot_be_reported_passed_from_normal_fallback_only() {
        let mut report = Report {
            passed: 2,
            total: 2,
            fallback: 2,
            ..Report::default()
        };
        assert!(report.succeeds(false));
        assert!(!report.succeeds(true));
        report.direct = 1;
        assert!(!report.succeeds(true));
        report.direct = 2;
        report.fallback = 0;
        assert!(report.succeeds(true));
        report.passed = 1;
        assert!(!report.succeeds(false));
    }
    fn image_server(responses: Vec<(u16, Vec<u8>)>) -> (String, std::thread::JoinHandle<usize>) {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = server.local_addr().unwrap();
        server.set_nonblocking(true).unwrap();
        let handle = std::thread::spawn(move || {
            let mut count = 0;
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            for (status, body) in responses {
                loop {
                    if std::time::Instant::now() >= deadline {
                        return count;
                    }
                    let Ok((mut stream, _)) = server.accept() else {
                        std::thread::sleep(Duration::from_millis(1));
                        continue;
                    };
                    stream
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    let mut request = Vec::new();
                    while request.len() < 8192
                        && !request.windows(4).any(|part| part == b"\r\n\r\n")
                    {
                        let mut buffer = [0; 1024];
                        let read = stream.read(&mut buffer).unwrap();
                        if read == 0 {
                            break;
                        }
                        request.extend_from_slice(&buffer[..read]);
                    }
                    write!(
                        stream,
                        "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .unwrap();
                    stream.write_all(&body).unwrap();
                    count += 1;
                    break;
                }
            }
            count
        });
        (format!("http://{addr}"), handle)
    }
    #[tokio::test]
    async fn connection_test_fetches_only_the_small_configured_sample_and_decodes_images() {
        let samples = sample_tiles(17);
        let replies = samples
            .iter()
            .enumerate()
            .map(|(i, _)| {
                (
                    200,
                    encoded(
                        if i % 2 == 0 {
                            ImageFormat::Png
                        } else {
                            ImageFormat::Jpeg
                        },
                        2,
                    ),
                )
            })
            .collect();
        let (url, server) = image_server(replies);
        let client = TileClient::from(reqwest::Client::builder().no_proxy().build().unwrap());
        let report = check(&client, &samples, &url).await;
        assert_eq!(server.join().unwrap(), samples.len());
        assert_eq!(report.passed, samples.len());
        assert_eq!(report.normal, samples.len());
        assert!(report.succeeds(false));
        assert!(!report.succeeds(true));
    }
    #[tokio::test]
    async fn http_failure_and_invalid_image_make_test_fail_without_retry_downloads() {
        let (url, server) = image_server(vec![
            (404, Vec::new()),
            (200, b"<html>not an image</html>".to_vec()),
        ]);
        let client = TileClient::from(reqwest::Client::builder().no_proxy().build().unwrap());
        let report = check(&client, &[(0, 0, 0), (1, 0, 0)], &url).await;
        assert_eq!(server.join().unwrap(), 2);
        assert_eq!(report.passed, 0);
        assert!(!report.succeeds(false));
    }
}
