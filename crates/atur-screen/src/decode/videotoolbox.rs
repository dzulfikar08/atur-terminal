//! H.264 decoding with `VideoToolbox` (hardware when the machine has it, the
//! system software decoder otherwise).
//!
//! Every access unit is decoded synchronously: the Annex B data becomes an AVCC
//! sample, `VTDecompressionSessionDecodeFrame` runs with flags 0 (no temporal
//! reordering, so pictures come out in decode order at once) and
//! `VTDecompressionSessionWaitForAsynchronousFrames` guarantees the output
//! callback has run before `decode` returns.
//!
//! This is the only module of the crate with unsafe code: the `VideoToolbox` and
//! `CoreMedia` C functions are declared by hand below (the `core-video` crate
//! covers `CoreVideo` only). Core Foundation objects are wrapped in
//! `core_foundation::base::CFType` (or the `core-video` types) right after
//! creation, so every "Create" rule reference is released exactly once.
#![allow(
    unsafe_code,
    reason = "VideoToolbox and CoreMedia have no safe bindings in the dependency tree"
)]
#![allow(
    non_snake_case,
    non_upper_case_globals,
    non_camel_case_types,
    reason = "declarations mirror the C headers"
)]

use std::ffi::c_void;
use std::ptr;
use std::sync::{Arc, Mutex, PoisonError};

use core_foundation::base::{CFAllocatorRef, CFType, CFTypeRef, TCFType, kCFAllocatorDefault};
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
use core_foundation::number::CFNumber;
use core_foundation::string::{CFString, CFStringRef};
use core_video::pixel_buffer::{
    CVPixelBuffer, CVPixelBufferKeys, kCVPixelBufferLock_ReadOnly,
    kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
};
use core_video::r#return::kCVReturnSuccess;

use super::nal::{self, NAL_IDR_SLICE, NAL_PPS, NAL_SLICE, NAL_SPS};
use super::yuv::nv12_to_bgra;
use super::{DecodeError, FrameSink, H264Decoder};
use crate::{Frame, Pixels, Surface};

type OSStatus = i32;

/// `CMTime`: value / timescale seconds. The header declares it with 4-byte
/// packing, which gives the same 24-byte layout as `repr(C)` on 64-bit targets.
#[repr(C)]
#[derive(Clone, Copy)]
struct CMTime {
    value: i64,
    timescale: i32,
    flags: u32,
    epoch: i64,
}

const CM_TIME_FLAGS_VALID: u32 = 1;
const CM_TIME_INVALID: CMTime = CMTime {
    value: 0,
    timescale: 0,
    flags: 0,
    epoch: 0,
};

/// `CMSampleTimingInfo`.
#[repr(C)]
struct CMSampleTimingInfo {
    duration: CMTime,
    presentation_time_stamp: CMTime,
    decode_time_stamp: CMTime,
}

/// `VTDecompressionOutputCallback`.
type OutputCallback = extern "C" fn(
    decompression_output_ref_con: *mut c_void,
    source_frame_ref_con: *mut c_void,
    status: OSStatus,
    info_flags: u32,
    image_buffer: *mut c_void,
    presentation_time_stamp: CMTime,
    presentation_duration: CMTime,
);

/// `VTDecompressionOutputCallbackRecord`.
#[repr(C)]
struct OutputCallbackRecord {
    decompression_output_callback: Option<OutputCallback>,
    decompression_output_ref_con: *mut c_void,
}

/// `kCMBlockBufferAssureMemoryNowFlag`.
const BLOCK_BUFFER_ASSURE_MEMORY_NOW: u32 = 1;

#[link(name = "CoreMedia", kind = "framework")]
#[allow(clippy::too_many_arguments, reason = "mirrors the C declaration")]
unsafe extern "C" {
    fn CMVideoFormatDescriptionCreateFromH264ParameterSets(
        allocator: CFAllocatorRef,
        parameter_set_count: usize,
        parameter_set_pointers: *const *const u8,
        parameter_set_sizes: *const usize,
        nal_unit_header_length: i32,
        format_description_out: *mut *const c_void,
    ) -> OSStatus;

    fn CMBlockBufferCreateWithMemoryBlock(
        structure_allocator: CFAllocatorRef,
        memory_block: *mut c_void,
        block_length: usize,
        block_allocator: CFAllocatorRef,
        custom_block_source: *const c_void,
        offset_to_data: usize,
        data_length: usize,
        flags: u32,
        block_buffer_out: *mut *const c_void,
    ) -> OSStatus;

    fn CMBlockBufferReplaceDataBytes(
        source_bytes: *const c_void,
        destination_buffer: *const c_void,
        offset_into_destination: usize,
        data_length: usize,
    ) -> OSStatus;

    fn CMSampleBufferCreate(
        allocator: CFAllocatorRef,
        data_buffer: *const c_void,
        data_ready: u8,
        make_data_ready_callback: *const c_void,
        make_data_ready_ref_con: *mut c_void,
        format_description: *const c_void,
        num_samples: isize,
        num_sample_timing_entries: isize,
        sample_timing_array: *const CMSampleTimingInfo,
        num_sample_size_entries: isize,
        sample_size_array: *const usize,
        sample_buffer_out: *mut *const c_void,
    ) -> OSStatus;
}

#[link(name = "VideoToolbox", kind = "framework")]
#[allow(clippy::too_many_arguments, reason = "mirrors the C declaration")]
unsafe extern "C" {
    fn VTDecompressionSessionCreate(
        allocator: CFAllocatorRef,
        video_format_description: *const c_void,
        video_decoder_specification: CFDictionaryRef,
        destination_image_buffer_attributes: CFDictionaryRef,
        output_callback: *const OutputCallbackRecord,
        decompression_session_out: *mut *mut c_void,
    ) -> OSStatus;

    fn VTDecompressionSessionDecodeFrame(
        session: *mut c_void,
        sample_buffer: *const c_void,
        decode_flags: u32,
        source_frame_ref_con: *mut c_void,
        info_flags_out: *mut u32,
    ) -> OSStatus;

    fn VTDecompressionSessionWaitForAsynchronousFrames(session: *mut c_void) -> OSStatus;

    fn VTDecompressionSessionInvalidate(session: *mut c_void);

    fn VTSessionSetProperty(
        session: *mut c_void,
        property_key: CFStringRef,
        property_value: CFTypeRef,
    ) -> OSStatus;
}

/// `kVTDecompressionPropertyKey_RealTime`. The constants of the `VideoToolbox`
/// headers are plain strings; spelling them out keeps the binary free of data
/// symbols that older macOS releases lack.
const KEY_REAL_TIME: &str = "RealTime";
/// `kVTVideoDecoderSpecification_EnableHardwareAcceleratedVideoDecoder`.
const KEY_ENABLE_HARDWARE: &str = "EnableHardwareAcceleratedVideoDecoder";

/// Timescale of the presentation stamps handed to `VideoToolbox` (one tick per access unit is enough).
const TIMESCALE: i32 = 1000;

/// What the output callback leaves for `decode` to pick up.
#[derive(Default)]
struct Outputs {
    /// The newest decoded picture (older ones are dropped).
    latest: Option<CVPixelBuffer>,
    /// The first error status the callback reported since the last pick-up.
    error: Option<OSStatus>,
}

/// Shared with the output callback through its refcon pointer.
#[derive(Default)]
struct CallbackState {
    outputs: Mutex<Outputs>,
}

/// Called by `VideoToolbox` once per decoded (or failed) picture, on the thread
/// that called `VTDecompressionSessionDecodeFrame` or on a `VideoToolbox` thread.
extern "C" fn output_callback(
    decompression_output_ref_con: *mut c_void,
    _source_frame_ref_con: *mut c_void,
    status: OSStatus,
    _info_flags: u32,
    image_buffer: *mut c_void,
    _presentation_time_stamp: CMTime,
    _presentation_duration: CMTime,
) {
    if decompression_output_ref_con.is_null() {
        return;
    }
    // SAFETY: the refcon is the `CallbackState` allocated in `Session::new`; it is
    // freed only after the session was invalidated, so it is alive for every callback.
    let state = unsafe { &*decompression_output_ref_con.cast::<CallbackState>() };
    let mut outputs = state.outputs.lock().unwrap_or_else(PoisonError::into_inner);
    if status != 0 {
        if outputs.error.is_none() {
            outputs.error = Some(status);
        }
        return;
    }
    if image_buffer.is_null() {
        // The decoder dropped the frame; `decode` reports it as "no picture".
        return;
    }
    // SAFETY: `image_buffer` is a valid CVPixelBuffer for the duration of the
    // callback. The "get" rule retains it, so the buffer outlives the callback.
    let buffer = unsafe { CVPixelBuffer::wrap_under_get_rule(image_buffer.cast()) };
    outputs.latest = Some(buffer);
}

fn status_error(what: &str, status: OSStatus) -> DecodeError {
    DecodeError(format!("VideoToolbox: {what} failed (OSStatus {status})"))
}

/// A `VTDecompressionSession` together with the state its callback writes to.
struct Session {
    /// Owned "create" reference, released in `Drop` after `Invalidate`.
    raw: *mut c_void,
    /// Keeps the format description alive for as long as the session lives.
    format: CFType,
    /// `Box::into_raw` of a `CallbackState`; freed in `Drop` once the session is gone.
    state: *mut CallbackState,
}

// SAFETY: a session is only used through `&mut` (or `&` for reading `state`) by one
// thread at a time, VideoToolbox sessions may be used from any thread, `state` is
// protected by its mutex and the CoreVideo buffers it holds are thread-safe,
// reference-counted objects.
unsafe impl Send for Session {}

impl Session {
    /// Creates a session for the given SPS and PPS NAL units (header byte included, no start codes).
    fn new(parameter_sets: &[Vec<u8>]) -> Result<Self, DecodeError> {
        let pointers: Vec<*const u8> = parameter_sets.iter().map(Vec::as_ptr).collect();
        let sizes: Vec<usize> = parameter_sets.iter().map(Vec::len).collect();
        let mut format_out: *const c_void = ptr::null();
        // SAFETY: `pointers` and `sizes` describe `parameter_sets.len()` valid
        // slices that outlive the call; the out-pointer is a valid local.
        let status = unsafe {
            CMVideoFormatDescriptionCreateFromH264ParameterSets(
                kCFAllocatorDefault,
                pointers.len(),
                pointers.as_ptr(),
                sizes.as_ptr(),
                4,
                &raw mut format_out,
            )
        };
        if status != 0 || format_out.is_null() {
            return Err(status_error("creating the format description", status));
        }
        // SAFETY: "Create" function, we own one reference.
        let format = unsafe { CFType::wrap_under_create_rule(format_out) };

        let attributes = destination_attributes()?;
        let specification = CFDictionary::<CFString, CFType>::from_CFType_pairs(&[(
            CFString::new(KEY_ENABLE_HARDWARE),
            CFBoolean::from(true).as_CFType(),
        )]);

        let state = Box::into_raw(Box::<CallbackState>::default());
        let record = OutputCallbackRecord {
            decompression_output_callback: Some(output_callback),
            decompression_output_ref_con: state.cast(),
        };
        let mut raw: *mut c_void = ptr::null_mut();
        // SAFETY: all references are valid for the call; VideoToolbox copies the
        // callback record and retains what it needs of the dictionaries.
        let status = unsafe {
            VTDecompressionSessionCreate(
                kCFAllocatorDefault,
                format.as_concrete_TypeRef(),
                specification.as_concrete_TypeRef(),
                attributes.as_concrete_TypeRef(),
                &raw const record,
                &raw mut raw,
            )
        };
        if status != 0 || raw.is_null() {
            // SAFETY: no session exists, so nothing else can reference the state.
            drop(unsafe { Box::from_raw(state) });
            return Err(status_error("creating the session", status));
        }
        let session = Self { raw, format, state };

        // Low latency is best effort: a decoder that does not know the key keeps working.
        let real_time = CFString::new(KEY_REAL_TIME);
        // SAFETY: the session and both CF objects are valid.
        let status = unsafe {
            VTSessionSetProperty(
                session.raw,
                real_time.as_concrete_TypeRef(),
                CFBoolean::from(true).as_CFTypeRef(),
            )
        };
        if status != 0 {
            log::debug!("VideoToolbox rejected the real-time property (OSStatus {status})");
        }
        Ok(session)
    }

    /// Decodes one AVCC access unit and returns what the callback produced.
    fn decode(&self, avcc: &[u8], pts: i64) -> Result<Option<CVPixelBuffer>, DecodeError> {
        let block = block_buffer(avcc)?;
        let sample = sample_buffer(&block, &self.format, avcc.len(), pts)?;

        let mut info_flags: u32 = 0;
        // SAFETY: session and sample buffer are valid; flags 0 decodes synchronously
        // in decode order; `info_flags` is a valid out-pointer.
        let status = unsafe {
            VTDecompressionSessionDecodeFrame(
                self.raw,
                sample.as_concrete_TypeRef(),
                0,
                ptr::null_mut(),
                &raw mut info_flags,
            )
        };
        // Wait even after an error so no callback is still running when we return.
        // SAFETY: the session is valid.
        let wait_status = unsafe { VTDecompressionSessionWaitForAsynchronousFrames(self.raw) };

        // SAFETY: `state` is alive as long as `self` (see `Drop`).
        let state = unsafe { &*self.state };
        let mut outputs = state.outputs.lock().unwrap_or_else(PoisonError::into_inner);
        let picture = outputs.latest.take();
        let callback_error = outputs.error.take();
        drop(outputs);

        if status != 0 {
            return Err(status_error("decoding", status));
        }
        if let Some(callback_status) = callback_error {
            return Err(status_error("decoding a frame", callback_status));
        }
        if wait_status != 0 {
            return Err(status_error("waiting for the decoder", wait_status));
        }
        Ok(picture)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: `raw` is a valid session we own; after the wait and the
        // invalidation VideoToolbox never calls back, so freeing `state` is sound.
        // `format` is released by its own drop afterwards.
        unsafe {
            VTDecompressionSessionWaitForAsynchronousFrames(self.raw);
            VTDecompressionSessionInvalidate(self.raw);
            core_foundation::base::CFRelease(self.raw.cast_const());
            drop(Box::from_raw(self.state));
        }
    }
}

/// The pixel buffer attributes every decoded picture must satisfy: the one
/// format GPUI's surface renderer accepts, backed by an `IOSurface` and usable by Metal.
fn destination_attributes() -> Result<CFDictionary<CFString, CFType>, DecodeError> {
    let format = i32::try_from(kCVPixelFormatType_420YpCbCr8BiPlanarFullRange)
        .map_err(|_| DecodeError("VideoToolbox: pixel format out of range".to_string()))?;
    let io_surface_properties = CFDictionary::<CFString, CFType>::from_CFType_pairs(&[]);
    Ok(CFDictionary::from_CFType_pairs(&[
        (
            CFString::from(CVPixelBufferKeys::PixelFormatType),
            CFNumber::from(format).as_CFType(),
        ),
        (
            CFString::from(CVPixelBufferKeys::MetalCompatibility),
            CFBoolean::from(true).as_CFType(),
        ),
        (
            CFString::from(CVPixelBufferKeys::IOSurfaceProperties),
            io_surface_properties.as_CFType(),
        ),
    ]))
}

/// Copies `data` into a new `CMBlockBuffer` that owns its memory.
fn block_buffer(data: &[u8]) -> Result<CFType, DecodeError> {
    let mut block_out: *const c_void = ptr::null();
    // SAFETY: with a null memory block CoreMedia allocates `data.len()` bytes with
    // the default allocator and owns them; the out-pointer is a valid local.
    let status = unsafe {
        CMBlockBufferCreateWithMemoryBlock(
            kCFAllocatorDefault,
            ptr::null_mut(),
            data.len(),
            kCFAllocatorDefault,
            ptr::null(),
            0,
            data.len(),
            BLOCK_BUFFER_ASSURE_MEMORY_NOW,
            &raw mut block_out,
        )
    };
    if status != 0 || block_out.is_null() {
        return Err(status_error("allocating a block buffer", status));
    }
    // SAFETY: "Create" function, we own one reference.
    let block = unsafe { CFType::wrap_under_create_rule(block_out) };
    // SAFETY: the block buffer holds `data.len()` writable bytes; `data` is valid for reads of that length.
    let status = unsafe {
        CMBlockBufferReplaceDataBytes(
            data.as_ptr().cast(),
            block.as_concrete_TypeRef(),
            0,
            data.len(),
        )
    };
    if status != 0 {
        return Err(status_error("filling a block buffer", status));
    }
    Ok(block)
}

/// Wraps a filled block buffer into a single-sample `CMSampleBuffer`.
fn sample_buffer(
    block: &CFType,
    format: &CFType,
    size: usize,
    pts: i64,
) -> Result<CFType, DecodeError> {
    let stamp = CMTime {
        value: pts,
        timescale: TIMESCALE,
        flags: CM_TIME_FLAGS_VALID,
        epoch: 0,
    };
    let timing = CMSampleTimingInfo {
        duration: CM_TIME_INVALID,
        presentation_time_stamp: stamp,
        decode_time_stamp: stamp,
    };
    let sizes = [size];
    let mut sample_out: *const c_void = ptr::null();
    // SAFETY: block buffer and format description are valid, the data is ready,
    // one timing and one size entry are passed for the single sample; the
    // arrays and the out-pointer are valid locals.
    let status = unsafe {
        CMSampleBufferCreate(
            kCFAllocatorDefault,
            block.as_concrete_TypeRef(),
            1,
            ptr::null(),
            ptr::null_mut(),
            format.as_concrete_TypeRef(),
            1,
            1,
            &raw const timing,
            1,
            sizes.as_ptr(),
            &raw mut sample_out,
        )
    };
    if status != 0 || sample_out.is_null() {
        return Err(status_error("creating a sample buffer", status));
    }
    // SAFETY: "Create" function, we own one reference.
    Ok(unsafe { CFType::wrap_under_create_rule(sample_out) })
}

/// Decodes H.264 with `VideoToolbox`.
pub(super) struct VideoToolboxDecoder {
    sink: FrameSink,
    session: Option<Session>,
    /// SPS and PPS NAL units the session was (or will be) built from.
    parameter_sets: Option<Vec<Vec<u8>>>,
    /// True until a key frame went through the current session.
    need_key_frame: bool,
    pts: i64,
}

impl VideoToolboxDecoder {
    pub(super) fn new(sink: FrameSink) -> Self {
        Self {
            sink,
            session: None,
            parameter_sets: None,
            need_key_frame: true,
            pts: 0,
        }
    }

    fn publish(&self, buffer: CVPixelBuffer) -> Result<(), DecodeError> {
        let (Ok(width), Ok(height)) = (
            u32::try_from(buffer.get_width()),
            u32::try_from(buffer.get_height()),
        ) else {
            return Err(DecodeError(
                "VideoToolbox: picture size out of range".to_string(),
            ));
        };
        let format = buffer.get_pixel_format();
        let pixels = if format == kCVPixelFormatType_420YpCbCr8BiPlanarFullRange {
            Pixels::Surface(Surface::new(buffer))
        } else if format == kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange {
            // GPUI only accepts full-range surfaces, so convert on the CPU.
            Pixels::Bgra(Arc::from(video_range_to_bgra(&buffer)?))
        } else {
            return Err(DecodeError(format!(
                "VideoToolbox: unsupported pixel format {format:#010x}"
            )));
        };
        (self.sink)(Frame {
            id: 0,
            width,
            height,
            pixels,
        });
        Ok(())
    }
}

impl H264Decoder for VideoToolboxDecoder {
    fn name(&self) -> String {
        "videotoolbox".to_string()
    }

    fn decode(&mut self, annex_b: &[u8], reset: bool) -> Result<(), DecodeError> {
        let nals = nal::split_annex_b(annex_b);
        if nals.is_empty() {
            return Err(DecodeError("H.264 data without NAL units".to_string()));
        }
        if reset {
            self.session = None;
            self.need_key_frame = true;
        }

        let mut sets: Vec<Vec<u8>> = Vec::new();
        for wanted in [NAL_SPS, NAL_PPS] {
            sets.extend(
                nals.iter()
                    .filter(|unit| nal::nal_type(unit) == Some(wanted))
                    .map(|unit| unit.to_vec()),
            );
        }
        let carries_sequence = nals.iter().any(|unit| nal::nal_type(unit) == Some(NAL_SPS));
        let carries_picture = nals.iter().any(|unit| nal::nal_type(unit) == Some(NAL_PPS));
        if carries_sequence && carries_picture && self.parameter_sets.as_ref() != Some(&sets) {
            self.parameter_sets = Some(sets);
            self.session = None;
            self.need_key_frame = true;
        }

        let slices: Vec<&[u8]> = nals
            .iter()
            .copied()
            .filter(|unit| matches!(nal::nal_type(unit), Some(NAL_SLICE | NAL_IDR_SLICE)))
            .collect();
        if slices.is_empty() {
            return Ok(());
        }
        let has_key_frame = slices
            .iter()
            .any(|unit| nal::nal_type(unit) == Some(NAL_IDR_SLICE));
        if self.need_key_frame && !has_key_frame {
            return Err(DecodeError("waiting for a key frame".to_string()));
        }

        if self.session.is_none() {
            let Some(parameter_sets) = self.parameter_sets.as_ref() else {
                return Err(DecodeError("no SPS/PPS received yet".to_string()));
            };
            self.session = Some(Session::new(parameter_sets)?);
        }
        let Some(session) = self.session.as_ref() else {
            return Err(DecodeError("VideoToolbox: no session".to_string()));
        };
        let Some(avcc) = nal::to_avcc(&slices) else {
            return Err(DecodeError("H.264 unit too large".to_string()));
        };

        self.pts = self.pts.wrapping_add(1);
        match session.decode(&avcc, self.pts) {
            Ok(picture) => {
                self.need_key_frame = false;
                match picture {
                    Some(buffer) => self.publish(buffer),
                    None => Ok(()),
                }
            }
            Err(error) => {
                // Rebuild the session and wait for a fresh key frame.
                self.session = None;
                self.need_key_frame = true;
                Err(error)
            }
        }
    }
}

/// Converts a `420v` (video range) buffer to packed BGRA.
fn video_range_to_bgra(buffer: &CVPixelBuffer) -> Result<Vec<u8>, DecodeError> {
    let fail = |what: &str| DecodeError(format!("VideoToolbox: {what}"));
    if buffer.get_plane_count() < 2 {
        return Err(fail("picture is not bi-planar"));
    }
    if buffer.lock_base_address(kCVPixelBufferLock_ReadOnly) != kCVReturnSuccess {
        return Err(fail("cannot lock the picture"));
    }
    let converted = locked_planes_to_bgra(buffer);
    buffer.unlock_base_address(kCVPixelBufferLock_ReadOnly);
    converted.ok_or_else(|| fail("cannot convert the picture"))
}

/// Reads the planes of a buffer that the caller has locked.
fn locked_planes_to_bgra(buffer: &CVPixelBuffer) -> Option<Vec<u8>> {
    let luma_stride = buffer.get_bytes_per_row_of_plane(0);
    let chroma_stride = buffer.get_bytes_per_row_of_plane(1);
    let luma_len = luma_stride.checked_mul(buffer.get_height_of_plane(0))?;
    let chroma_len = chroma_stride.checked_mul(buffer.get_height_of_plane(1))?;
    // SAFETY: the buffer is locked, so the plane addresses are valid for
    // `bytes per row * rows` bytes until the caller unlocks it.
    let (luma, chroma) = unsafe {
        let luma = buffer
            .get_base_address_of_plane(0)
            .cast::<u8>()
            .cast_const();
        let chroma = buffer
            .get_base_address_of_plane(1)
            .cast::<u8>()
            .cast_const();
        if luma.is_null() || chroma.is_null() {
            return None;
        }
        (
            std::slice::from_raw_parts(luma, luma_len),
            std::slice::from_raw_parts(chroma, chroma_len),
        )
    };
    nv12_to_bgra(
        buffer.get_width(),
        buffer.get_height(),
        luma,
        luma_stride,
        chroma,
        chroma_stride,
        false,
    )
}
