//! CoreVideo pixel buffer FFI for extracting image data from CMSampleBuffer.
//!
//! Provides safe wrappers around CoreVideo C functions to lock, read, and
//! release CVPixelBuffer data. The `PixelBufferGuard` RAII type ensures the
//! buffer is unlocked on drop.

use std::ffi::c_void;

use objc2_core_media::CMSampleBuffer;

use crate::error::{CaptureError, Result};

// ---------------------------------------------------------------------------
// CoreMedia / CoreVideo FFI
// ---------------------------------------------------------------------------

#[link(name = "CoreMedia", kind = "framework")]
unsafe extern "C" {
    fn CMSampleBufferGetImageBuffer(sbuf: *const c_void) -> *mut c_void;
}

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    fn CVPixelBufferGetWidth(pixel_buffer: *mut c_void) -> usize;
    fn CVPixelBufferGetHeight(pixel_buffer: *mut c_void) -> usize;
    fn CVPixelBufferGetBytesPerRow(pixel_buffer: *mut c_void) -> usize;
    fn CVPixelBufferGetPixelFormatType(pixel_buffer: *mut c_void) -> u32;
    fn CVPixelBufferLockBaseAddress(pixel_buffer: *mut c_void, lock_flags: u64) -> i32;
    fn CVPixelBufferUnlockBaseAddress(pixel_buffer: *mut c_void, lock_flags: u64) -> i32;
    fn CVPixelBufferGetBaseAddress(pixel_buffer: *mut c_void) -> *mut c_void;
}

/// Read-only lock flag for CVPixelBuffer.
const K_CV_PIXEL_BUFFER_LOCK_READ_ONLY: u64 = 0x0000_0001;

/// Extract the CVPixelBuffer (CVImageBufferRef) from a CMSampleBuffer.
///
/// Returns `None` if the sample buffer contains no image data (e.g., audio-only).
pub fn get_image_buffer(sample_buffer: &CMSampleBuffer) -> Option<*mut c_void> {
    let ptr = sample_buffer as *const CMSampleBuffer as *const c_void;
    let image_buf = unsafe { CMSampleBufferGetImageBuffer(ptr) };
    if image_buf.is_null() {
        None
    } else {
        Some(image_buf)
    }
}

/// Query the pixel format type (FourCC) of a CVPixelBuffer.
///
/// Common values: `BGRA` = 0x42475241 (big-endian "BGRA").
pub fn pixel_format(pixel_buffer: *mut c_void) -> u32 {
    unsafe { CVPixelBufferGetPixelFormatType(pixel_buffer) }
}

/// Query the width of a CVPixelBuffer in pixels (no lock required).
pub fn width(pixel_buffer: *mut c_void) -> usize {
    unsafe { CVPixelBufferGetWidth(pixel_buffer) }
}

/// Query the height of a CVPixelBuffer in pixels (no lock required).
pub fn height(pixel_buffer: *mut c_void) -> usize {
    unsafe { CVPixelBufferGetHeight(pixel_buffer) }
}

/// RAII guard that locks a CVPixelBuffer on creation and unlocks on drop.
///
/// Provides safe read-only access to the pixel data while the lock is held.
pub struct PixelBufferGuard {
    pixel_buffer: *mut c_void,
    width: usize,
    height: usize,
    bytes_per_row: usize,
    base_address: *const u8,
    data_len: usize,
}

impl PixelBufferGuard {
    /// Lock the pixel buffer for read-only CPU access.
    ///
    /// # Errors
    ///
    /// Returns `CaptureError::Encoding` if the lock fails or the base address is null.
    pub fn new(pixel_buffer: *mut c_void) -> Result<Self> {
        let status =
            unsafe { CVPixelBufferLockBaseAddress(pixel_buffer, K_CV_PIXEL_BUFFER_LOCK_READ_ONLY) };
        if status != 0 {
            return Err(CaptureError::Encoding(format!(
                "failed to lock pixel buffer: CVReturn {status}"
            )));
        }

        let base = unsafe { CVPixelBufferGetBaseAddress(pixel_buffer) };
        if base.is_null() {
            // Unlock before returning the error.
            unsafe {
                CVPixelBufferUnlockBaseAddress(pixel_buffer, K_CV_PIXEL_BUFFER_LOCK_READ_ONLY);
            }
            return Err(CaptureError::Encoding(
                "pixel buffer base address is null".into(),
            ));
        }

        let width = unsafe { CVPixelBufferGetWidth(pixel_buffer) };
        let height = unsafe { CVPixelBufferGetHeight(pixel_buffer) };
        let bytes_per_row = unsafe { CVPixelBufferGetBytesPerRow(pixel_buffer) };
        let data_len = height
            .checked_mul(bytes_per_row)
            .filter(|len| *len <= isize::MAX as usize)
            .ok_or_else(|| {
                // Unlock before returning.
                unsafe {
                    CVPixelBufferUnlockBaseAddress(
                        pixel_buffer,
                        K_CV_PIXEL_BUFFER_LOCK_READ_ONLY,
                    );
                }
                CaptureError::Encoding(
                    "pixel buffer size overflow while computing data_len".into(),
                )
            })?;

        Ok(Self {
            pixel_buffer,
            width,
            height,
            bytes_per_row,
            base_address: base as *const u8,
            data_len,
        })
    }

    /// Width of the pixel buffer in pixels.
    pub fn width(&self) -> usize {
        self.width
    }

    /// Height of the pixel buffer in pixels.
    pub fn height(&self) -> usize {
        self.height
    }

    /// Number of bytes per row (may include padding).
    pub fn bytes_per_row(&self) -> usize {
        self.bytes_per_row
    }

    /// View the locked pixel data as a byte slice.
    ///
    /// The returned slice is valid for the lifetime of this guard.
    pub fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.base_address, self.data_len) }
    }
}

impl Drop for PixelBufferGuard {
    fn drop(&mut self) {
        unsafe {
            CVPixelBufferUnlockBaseAddress(self.pixel_buffer, K_CV_PIXEL_BUFFER_LOCK_READ_ONLY);
        }
    }
}

// SAFETY: CVPixelBuffer data is thread-safe when locked read-only.
// The guard holds a read lock and only provides immutable access via as_slice().
// Only Send is needed (not Sync) because the guard is used within a single
// async task — it's never shared across threads, only moved.
unsafe impl Send for PixelBufferGuard {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_lock_flag_is_correct() {
        assert_eq!(K_CV_PIXEL_BUFFER_LOCK_READ_ONLY, 1);
    }
}
