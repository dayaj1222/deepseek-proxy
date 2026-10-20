//! Image metadata: format detection and pixel dimensions.
//!
//! Port of `aiodeepseek/types/models/_model_utils.py`. DeepSeek only accepts
//! PNG and JPEG, and the upload request needs the real dimensions, so both are
//! read from the bytes rather than trusted from the client.

/// Detected format plus the dimensions read from the header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageMeta {
    pub mime: &'static str,
    pub width: u32,
    pub height: u32,
}

/// Detect the format and read `(width, height)`.
///
/// Errors carry the Python message so the proxy can surface it unchanged.
pub fn meta(data: &[u8]) -> Result<ImageMeta, String> {
    let mime = detect_mime(data)?;
    let (width, height) = if mime == "image/png" {
        png_dimensions(data)?
    } else {
        jpeg_dimensions(data)
    };
    Ok(ImageMeta {
        mime,
        width,
        height,
    })
}

fn detect_mime(data: &[u8]) -> Result<&'static str, String> {
    if data.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        return Ok("image/png");
    }
    if data.starts_with(&[0xFF, 0xD8]) {
        return Ok("image/jpeg");
    }
    Err("Unsupported image format; only PNG and JPEG are accepted".into())
}

/// PNG IHDR width/height live at bytes 16..24 (big endian).
fn png_dimensions(data: &[u8]) -> Result<(u32, u32), String> {
    let ihdr = data
        .get(16..24)
        .ok_or_else(|| "truncated PNG header".to_string())?;
    let width = u32::from_be_bytes([ihdr[0], ihdr[1], ihdr[2], ihdr[3]]);
    let height = u32::from_be_bytes([ihdr[4], ihdr[5], ihdr[6], ihdr[7]]);
    Ok((width, height))
}

/// Scan JPEG segment markers for a SOF0/1/2 frame header.
///
/// Returns `(0, 0)` when no frame header is found, matching Python: the
/// dimensions are advisory, the upload itself does not need them.
fn jpeg_dimensions(data: &[u8]) -> (u32, u32) {
    let mut i = 2usize;
    while i + 4 < data.len() {
        if data[i] != 0xFF {
            break;
        }
        let marker = data[i + 1];
        if matches!(marker, 0xC0 | 0xC1 | 0xC2) {
            let h = u16::from_be_bytes([data[i + 5], data[i + 6]]) as u32;
            let w = u16::from_be_bytes([data[i + 7], data[i + 8]]) as u32;
            return (w, h);
        }
        let segment_len = u16::from_be_bytes([data[i + 2], data[i + 3]]) as usize;
        i += 2 + segment_len;
    }
    (0, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smallest PNG header that carries the IHDR dimensions.
    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut v = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        v.extend_from_slice(&[0, 0, 0, 13]);
        v.extend_from_slice(b"IHDR");
        v.extend_from_slice(&width.to_be_bytes());
        v.extend_from_slice(&height.to_be_bytes());
        v
    }

    #[test]
    fn reads_png_dimensions() {
        let m = meta(&png(64, 48)).unwrap();
        assert_eq!(m.mime, "image/png");
        assert_eq!((m.width, m.height), (64, 48));
    }

    #[test]
    fn reads_jpeg_dimensions_from_sof0() {
        let mut v = vec![0xFF, 0xD8];
        // APP0 segment, length 4 (includes the length bytes).
        v.extend_from_slice(&[0xFF, 0xE0, 0x00, 0x04, 0x00, 0x00]);
        // SOF0: marker, length, precision, height, width.
        v.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x11, 0x08]);
        v.extend_from_slice(&720u16.to_be_bytes());
        v.extend_from_slice(&1280u16.to_be_bytes());
        let m = meta(&v).unwrap();
        assert_eq!(m.mime, "image/jpeg");
        assert_eq!((m.width, m.height), (1280, 720));
    }

    #[test]
    fn rejects_other_formats() {
        let err = meta(b"GIF89a").unwrap_err();
        assert!(err.contains("only PNG and JPEG"));
    }
}
