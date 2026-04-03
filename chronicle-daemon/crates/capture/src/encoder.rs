//! HEIF encoding via macOS ImageIO framework.
//!
//! Converts `CMSampleBuffer` frames from ScreenCaptureKit into HEIF files
//! on disk. Uses hardware-accelerated HEVC encoding on Apple Silicon.

use std::ffi::c_void;
use std::path::Path;
use std::ptr;

use core_foundation::base::TCFType;
use core_foundation::dictionary::CFDictionary;
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;
use core_foundation::url::CFURL;
use core_graphics::color_space::CGColorSpace;
use core_graphics::data_provider::CGDataProvider;
use core_graphics::image::CGImage;
use foreign_types::ForeignType;

use objc2_core_media::CMSampleBuffer;

use crate::error::{CaptureError, Result};
use crate::pixel_buffer;

// ---------------------------------------------------------------------------
// ImageIO FFI — these symbols live in the ImageIO framework and are not
// wrapped by any maintained Rust crate.
// ---------------------------------------------------------------------------

#[link(name = "ImageIO", kind = "framework")]
unsafe extern "C" {
    fn CGImageDestinationCreateWithURL(
        url: *const c_void,      // CFURLRef
        uti: *const c_void,      // CFStringRef
        count: usize,
        options: *const c_void,  // CFDictionaryRef, nullable
    ) -> *mut c_void;            // CGImageDestinationRef

    fn CGImageDestinationAddImage(
        dest: *mut c_void,       // CGImageDestinationRef
        image: *const c_void,    // CGImageRef
        properties: *const c_void, // CFDictionaryRef, nullable
    );

    fn CGImageDestinationFinalize(dest: *mut c_void) -> bool;

    static kCGImageDestinationLossyCompressionQuality: *const c_void; // CFStringRef
}

// CoreFoundation release — needed for the CGImageDestination which is
// returned as an untyped pointer.
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(cf: *const c_void);
}

/// Encode a `CMSampleBuffer` as HEIF and write to disk.
///
/// Extracts the `CVPixelBuffer` from the sample buffer, builds a `CGImage`
/// from the raw BGRA pixel data, and writes it as HEIF via ImageIO.
///
/// The sample buffer **must** contain BGRA pixel data (as produced by
/// `CaptureEngine`, which configures BGRA pixel format). Other pixel
/// formats will produce corrupt output.
///
/// # Arguments
/// * `sample_buffer` -- raw frame from ScreenCaptureKit (BGRA format)
/// * `output_path` -- destination file path (parent directory must exist)
/// * `quality` -- compression quality 0.0-1.0 (recommended: 0.65)
///
/// # Errors
/// Returns `CaptureError::Encoding` if any step fails.
pub fn encode_heif(
    sample_buffer: &CMSampleBuffer,
    output_path: &Path,
    quality: f64,
) -> Result<()> {
    if !(0.0..=1.0).contains(&quality) {
        return Err(CaptureError::Encoding(
            "quality must be between 0.0 and 1.0".into(),
        ));
    }

    // 1. Extract pixel buffer from sample buffer.
    let px_buf = pixel_buffer::get_image_buffer(sample_buffer)
        .ok_or_else(|| CaptureError::Encoding("failed to extract pixel buffer".into()))?;

    // 2. Validate pixel format is BGRA.
    const BGRA_FOURCC: u32 = u32::from_be_bytes(*b"BGRA");
    let format = pixel_buffer::pixel_format(px_buf);
    if format != BGRA_FOURCC {
        return Err(CaptureError::Encoding(format!(
            "expected BGRA pixel format, got 0x{format:08X}"
        )));
    }

    // 3. Lock pixel buffer for read-only CPU access.
    let guard = pixel_buffer::PixelBufferGuard::new(px_buf)?;

    let width = guard.width();
    let height = guard.height();
    let bytes_per_row = guard.bytes_per_row();
    let raw_bytes = guard.as_slice();

    // 4. Create CGImage from raw BGRA pixel data.
    let cg_image = create_cgimage_from_bgra(raw_bytes, width, height, bytes_per_row)?;

    // 5. Write as HEIF.
    write_cgimage_as_heif(&cg_image, output_path, quality)
}

/// Create a `CGImage` from raw BGRA pixel data.
///
/// ScreenCaptureKit delivers frames in BGRA format (configured via
/// BGRA pixel format in the stream configuration).
fn create_cgimage_from_bgra(
    data: &[u8],
    width: usize,
    height: usize,
    bytes_per_row: usize,
) -> Result<CGImage> {
    let required_len = height.saturating_mul(bytes_per_row);
    if data.len() < required_len {
        return Err(CaptureError::Encoding(format!(
            "data slice too small: {} < {}",
            data.len(),
            required_len
        )));
    }

    let color_space = CGColorSpace::create_device_rgb();
    // SAFETY: data is borrowed from a locked CVPixelBuffer guard and remains
    // valid for the lifetime of this function call.
    let provider = unsafe { CGDataProvider::from_slice(data) };

    // BGRA in memory = kCGBitmapByteOrder32Little | kCGImageAlphaPremultipliedFirst
    // Numeric: 0x2000 (byte order 32 little) | 0x2 (premultiplied first) = 0x2002
    let bitmap_info: u32 = 0x2002;

    // CGImage::new panics on null (asserts internally), which only happens
    // if the parameters are fundamentally invalid (zero dimensions, etc.).
    Ok(CGImage::new(
        width,
        height,
        8,              // bits per component
        32,             // bits per pixel
        bytes_per_row,
        &color_space,
        bitmap_info,
        &provider,
        false,          // should_interpolate
        0,              // rendering intent: default
    ))
}

/// Write a `CGImage` as HEIF to the given path.
///
/// This is the core ImageIO interaction. Separated from `encode_heif` so it
/// can be unit-tested with synthetic CGImages (no ScreenCaptureKit needed).
fn write_cgimage_as_heif(image: &CGImage, path: &Path, quality: f64) -> Result<()> {
    let url = CFURL::from_path(path, false)
        .ok_or_else(|| CaptureError::Encoding("failed to create URL from path".into()))?;

    // Apple uses "public.heic" (HEIC = High Efficiency Image Container)
    // as the UTI for HEIF images. "public.heif" is not accepted by
    // CGImageDestination.
    let uti = CFString::new("public.heic");

    // Create destination.
    let dest = unsafe {
        CGImageDestinationCreateWithURL(
            url.as_concrete_TypeRef() as *const c_void,
            uti.as_concrete_TypeRef() as *const c_void,
            1,
            ptr::null(),
        )
    };
    if dest.is_null() {
        return Err(CaptureError::Encoding(
            "failed to create image destination".into(),
        ));
    }

    // Build properties dictionary with compression quality.
    let quality_value = CFNumber::from(quality as f32);
    let properties = unsafe {
        let key = kCGImageDestinationLossyCompressionQuality;
        CFDictionary::from_CFType_pairs(&[(
            CFString::wrap_under_get_rule(key as *const _),
            quality_value.as_CFType(),
        )])
    };

    // Add image and finalize.
    unsafe {
        CGImageDestinationAddImage(
            dest,
            image.as_ptr() as *const c_void,
            properties.as_concrete_TypeRef() as *const c_void,
        );

        let ok = CGImageDestinationFinalize(dest);
        CFRelease(dest as *const c_void);

        if !ok {
            return Err(CaptureError::Encoding("failed to finalize HEIF output".into()));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Create synthetic BGRA pixel data (solid red, 100x100) for testing.
    /// Returns the data alongside dimensions so the caller keeps it alive
    /// for the lifetime of any CGImage created from it (CGDataProvider::from_slice
    /// borrows, it does not own).
    fn make_test_pixel_data() -> (Vec<u8>, usize, usize, usize) {
        let width = 100;
        let height = 100;
        let bytes_per_row = width * 4;
        let mut data = vec![0u8; height * bytes_per_row];
        for pixel in data.chunks_exact_mut(4) {
            pixel[0] = 0;   // B
            pixel[1] = 0;   // G
            pixel[2] = 255; // R
            pixel[3] = 255; // A
        }
        (data, width, height, bytes_per_row)
    }

    #[test]
    fn create_cgimage_from_bgra_valid_data() {
        let width = 50;
        let height = 50;
        let bytes_per_row = width * 4;
        let data = vec![128u8; height * bytes_per_row];
        let image = create_cgimage_from_bgra(&data, width, height, bytes_per_row);
        assert!(image.is_ok());
        let img = image.unwrap();
        assert_eq!(img.width(), width);
        assert_eq!(img.height(), height);
    }

    #[test]
    fn write_heif_creates_file() {
        let (data, w, h, bpr) = make_test_pixel_data();
        let image = create_cgimage_from_bgra(&data, w, h, bpr).unwrap();
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        let path = dir.path().join("test.heif");

        let result = write_cgimage_as_heif(&image, &path, 0.65);
        assert!(result.is_ok(), "write_cgimage_as_heif failed: {result:?}");
        assert!(path.exists(), "HEIF file was not created");

        let metadata = fs::metadata(&path).unwrap();
        assert!(metadata.len() > 0, "HEIF file is empty");
    }

    #[test]
    fn write_heif_has_valid_magic_bytes() {
        let (pixels, w, h, bpr) = make_test_pixel_data();
        let image = create_cgimage_from_bgra(&pixels, w, h, bpr).unwrap();
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        let path = dir.path().join("magic.heif");

        write_cgimage_as_heif(&image, &path, 0.65).unwrap();

        let data = fs::read(&path).unwrap();
        // HEIF files contain "ftyp" in the first 12 bytes.
        assert!(data.len() >= 12, "file too small for HEIF");
        let ftyp_pos = data[..12]
            .windows(4)
            .position(|w| w == b"ftyp");
        assert!(ftyp_pos.is_some(), "missing ftyp box in HEIF header");
    }

    #[test]
    fn write_heif_quality_affects_size() {
        // Use high-entropy pixels so the encoder actually has to work harder
        // at higher quality. Solid-color images compress identically at any
        // quality level, which would let a non-strict assertion pass vacuously.
        let w = 256;
        let h = 256;
        let bpr = w * 4;
        let mut data = vec![0u8; h * bpr];
        for (i, pixel) in data.chunks_exact_mut(4).enumerate() {
            let x = (i % w) as u8;
            let y = (i / w) as u8;
            pixel[0] = x.wrapping_mul(31);
            pixel[1] = y.wrapping_mul(17);
            pixel[2] = x ^ y;
            pixel[3] = 255;
        }
        let image = create_cgimage_from_bgra(&data, w, h, bpr).unwrap();
        let dir = tempfile::tempdir().expect("failed to create temp dir");

        let low_path = dir.path().join("low.heif");
        let high_path = dir.path().join("high.heif");

        write_cgimage_as_heif(&image, &low_path, 0.1).unwrap();
        write_cgimage_as_heif(&image, &high_path, 1.0).unwrap();

        let low_size = fs::metadata(&low_path).unwrap().len();
        let high_size = fs::metadata(&high_path).unwrap().len();

        assert!(
            high_size > low_size,
            "expected high quality ({high_size}) > low quality ({low_size})"
        );
    }

    #[test]
    fn write_heif_accepts_edge_quality_values() {
        let (data, w, h, bpr) = make_test_pixel_data();
        let image = create_cgimage_from_bgra(&data, w, h, bpr).unwrap();
        let dir = tempfile::tempdir().unwrap();

        assert!(write_cgimage_as_heif(&image, &dir.path().join("zero.heif"), 0.0).is_ok());
        assert!(write_cgimage_as_heif(&image, &dir.path().join("one.heif"), 1.0).is_ok());
    }

    #[test]
    fn write_heif_invalid_path_returns_error() {
        let (data, w, h, bpr) = make_test_pixel_data();
        let image = create_cgimage_from_bgra(&data, w, h, bpr).unwrap();
        let path = Path::new("/nonexistent/directory/out.heif");
        let result = write_cgimage_as_heif(&image, &path, 0.65);
        assert!(result.is_err());
    }
}
