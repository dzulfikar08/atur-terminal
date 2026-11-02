//! H.264 encoding with `VideoToolbox`, on the Mac's video hardware when it has
//! some: real-time, no B-frames, one access unit per picture, a key frame
//! whenever asked.
//!
//! Every picture is encoded synchronously: `VTCompressionSessionEncodeFrame`
//! followed by `VTCompressionSessionCompleteFrames`, so the output callback has
//! run before [`VideoToolboxEncoder::encode`] returns and nothing queues up.
#![allow(
    unsafe_code,
    reason = "VideoToolbox and CoreMedia have no safe bindings in the dependency tree"
)]
#![allow(non_upper_case_globals, reason = "declarations mirror the C headers")]

use std::ffi::c_void;
use std::ptr;
use std::sync::{Mutex, PoisonError};
use std::time::Instant;

use core_foundation::array::CFArray;
use core_foundation::base::{CFType, CFTypeRef, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::CFDictionary;
use core_foundation::number::CFNumber;
use core_foundation::string::{CFString, CFStringRef};

use super::pixel::PixelBuffer;
use crate::encoder_config::{EncoderConfig, Profile};
use crate::frame::{Frame, FrameData};
use crate::h264;
use crate::serve::Encoder;

type OsStatus = i32;
type SessionRef = *mut c_void;

/// `kCMVideoCodecType_H264`: 'avc1'.
const CODEC_H264: u32 = 0x6176_6331;
const TIMESCALE: i32 = 1000;
const TIME_FLAGS_VALID: u32 = 1;

/// `CMTime`: value / timescale seconds; 24 bytes as in the header.
#[repr(C)]
#[derive(Clone, Copy)]
struct CmTime {
    value: i64,
    timescale: i32,
    flags: u32,
    epoch: i64,
}

const TIME_INVALID: CmTime = CmTime {
    value: 0,
    timescale: 0,
    flags: 0,
    epoch: 0,
};

type OutputCallback = extern "C" fn(
    output_ref_con: *mut c_void,
    source_frame_ref_con: *mut c_void,
    status: OsStatus,
    info_flags: u32,
    sample_buffer: *const c_void,
);

#[link(name = "CoreMedia", kind = "framework")]
unsafe extern "C" {
    static kCMSampleAttachmentKey_NotSync: CFStringRef;

    fn CMSampleBufferGetDataBuffer(sample: *const c_void) -> *const c_void;
    fn CMSampleBufferGetFormatDescription(sample: *const c_void) -> *const c_void;
    fn CMSampleBufferGetSampleAttachmentsArray(
        sample: *const c_void,
        create_if_necessary: u8,
    ) -> *const c_void;
    fn CMBlockBufferGetDataLength(buffer: *const c_void) -> usize;
    fn CMBlockBufferCopyDataBytes(
        buffer: *const c_void,
        offset: usize,
        length: usize,
        destination: *mut c_void,
    ) -> OsStatus;
    fn CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
        description: *const c_void,
        index: usize,
        parameter_set_out: *mut *const u8,
        size_out: *mut usize,
        count_out: *mut usize,
        nal_unit_header_length_out: *mut i32,
    ) -> OsStatus;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFArrayGetCount(array: *const c_void) -> isize;
    fn CFArrayGetValueAtIndex(array: *const c_void, index: isize) -> *const c_void;
    fn CFDictionaryGetValue(dictionary: *const c_void, key: *const c_void) -> *const c_void;
    fn CFBooleanGetValue(boolean: *const c_void) -> u8;
    fn CFRelease(object: *const c_void);
}

#[link(name = "VideoToolbox", kind = "framework")]
#[allow(clippy::too_many_arguments, reason = "mirrors the C declaration")]
unsafe extern "C" {
    static kVTCompressionPropertyKey_RealTime: CFStringRef;
    static kVTCompressionPropertyKey_ProfileLevel: CFStringRef;
    static kVTCompressionPropertyKey_AllowFrameReordering: CFStringRef;
    static kVTCompressionPropertyKey_AverageBitRate: CFStringRef;
    static kVTCompressionPropertyKey_DataRateLimits: CFStringRef;
    static kVTCompressionPropertyKey_MaxKeyFrameInterval: CFStringRef;
    static kVTCompressionPropertyKey_ExpectedFrameRate: CFStringRef;
    static kVTCompressionPropertyKey_ColorPrimaries: CFStringRef;
    static kVTCompressionPropertyKey_TransferFunction: CFStringRef;
    static kVTCompressionPropertyKey_YCbCrMatrix: CFStringRef;
    static kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality: CFStringRef;
    static kVTEncodeFrameOptionKey_ForceKeyFrame: CFStringRef;
    static kVTVideoEncoderSpecification_EnableLowLatencyRateControl: CFStringRef;
    static kVTProfileLevel_H264_High_AutoLevel: CFStringRef;
    static kVTProfileLevel_H264_Main_AutoLevel: CFStringRef;

    fn VTCompressionSessionCreate(
        allocator: *const c_void,
        width: i32,
        height: i32,
        codec_type: u32,
        encoder_specification: *const c_void,
        source_image_buffer_attributes: *const c_void,
        compressed_data_allocator: *const c_void,
        output_callback: OutputCallback,
        output_callback_ref_con: *mut c_void,
        compression_session_out: *mut SessionRef,
    ) -> OsStatus;
    fn VTCompressionSessionPrepareToEncodeFrames(session: SessionRef) -> OsStatus;
    fn VTCompressionSessionEncodeFrame(
        session: SessionRef,
        image_buffer: *mut c_void,
        presentation_time_stamp: CmTime,
        duration: CmTime,
        frame_properties: *const c_void,
        source_frame_ref_con: *mut c_void,
        info_flags_out: *mut u32,
    ) -> OsStatus;
    fn VTCompressionSessionCompleteFrames(
        session: SessionRef,
        complete_until_presentation_time_stamp: CmTime,
    ) -> OsStatus;
    fn VTCompressionSessionInvalidate(session: SessionRef);
    fn VTSessionSetProperty(
        session: SessionRef,
        property_key: CFStringRef,
        property_value: CFTypeRef,
    ) -> OsStatus;
}

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    static kCVImageBufferColorPrimaries_SMPTE_C: CFStringRef;
    static kCVImageBufferTransferFunction_ITU_R_709_2: CFStringRef;
    static kCVImageBufferYCbCrMatrix_ITU_R_601_4: CFStringRef;
}

/// What the output callback collects for [`VideoToolboxEncoder::encode`].
#[derive(Default)]
struct Sink {
    units: Mutex<Vec<Vec<u8>>>,
    error: Mutex<Option<String>>,
}

impl Sink {
    fn fail(&self, reason: String) {
        let mut error = self.error.lock().unwrap_or_else(PoisonError::into_inner);
        error.get_or_insert(reason);
    }
}

extern "C" fn output_callback(
    output_ref_con: *mut c_void,
    _source_frame_ref_con: *mut c_void,
    status: OsStatus,
    _info_flags: u32,
    sample_buffer: *const c_void,
) {
    if output_ref_con.is_null() {
        return;
    }
    // SAFETY: the refcon is the `Sink` boxed by `VideoToolboxEncoder::new`, which
    // outlives the session: the session is invalidated before the box is dropped.
    let sink = unsafe { &*output_ref_con.cast_const().cast::<Sink>() };
    if status != 0 {
        sink.fail(format!("VideoToolbox reported OSStatus {status}"));
        return;
    }
    if sample_buffer.is_null() {
        return;
    }
    // SAFETY: `sample_buffer` is valid for the duration of the callback.
    match unsafe { access_unit(sample_buffer) } {
        Ok(unit) => sink
            .units
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(unit),
        Err(reason) => sink.fail(reason),
    }
}

/// The Annex B access unit in an encoded sample.
///
/// # Safety
/// `sample` must be a valid `CMSampleBufferRef`.
unsafe fn access_unit(sample: *const c_void) -> Result<Vec<u8>, String> {
    // SAFETY: the caller guarantees `sample` is valid; the block buffer belongs to it.
    unsafe {
        let block = CMSampleBufferGetDataBuffer(sample);
        if block.is_null() {
            return Err("the encoded sample has no data".into());
        }
        let length = CMBlockBufferGetDataLength(block);
        let mut data = vec![0_u8; length];
        let status = CMBlockBufferCopyDataBytes(block, 0, length, data.as_mut_ptr().cast());
        if status != 0 {
            return Err(format!(
                "copying the encoded data failed (OSStatus {status})"
            ));
        }
        let key = is_key_frame(sample);
        let description = CMSampleBufferGetFormatDescription(sample);
        let (sets, header) = if key {
            parameter_sets(description)?
        } else {
            (Vec::new(), 4)
        };
        let borrowed: Vec<&[u8]> = sets.iter().map(Vec::as_slice).collect();
        h264::annex_b(&data, header, &borrowed, key)
    }
}

/// Whether the sample is a sync (key) frame: it is unless marked `NotSync`.
///
/// # Safety
/// `sample` must be a valid `CMSampleBufferRef`.
unsafe fn is_key_frame(sample: *const c_void) -> bool {
    // SAFETY: the attachments array belongs to the sample; the key is a constant.
    unsafe {
        let attachments = CMSampleBufferGetSampleAttachmentsArray(sample, 0);
        if attachments.is_null() || CFArrayGetCount(attachments) < 1 {
            return true;
        }
        let first = CFArrayGetValueAtIndex(attachments, 0);
        let not_sync = CFDictionaryGetValue(first, kCMSampleAttachmentKey_NotSync.cast());
        not_sync.is_null() || CFBooleanGetValue(not_sync) == 0
    }
}

/// The SPS and PPS of a sample's format and the NAL length prefix size.
///
/// # Safety
/// `description` must be a valid `CMFormatDescriptionRef`, or null.
unsafe fn parameter_sets(description: *const c_void) -> Result<(Vec<Vec<u8>>, usize), String> {
    if description.is_null() {
        return Err("the encoded sample has no format description".into());
    }
    let mut sets = Vec::new();
    let mut count = 0_usize;
    let mut header = 4_i32;
    let mut index = 0;
    while index < count.max(1) {
        let mut data: *const u8 = ptr::null();
        let mut size = 0_usize;
        // SAFETY: the description is valid and the out pointers are valid.
        let status = unsafe {
            CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                description,
                index,
                &raw mut data,
                &raw mut size,
                &raw mut count,
                &raw mut header,
            )
        };
        if status != 0 || data.is_null() {
            return Err(format!(
                "reading the parameter sets failed (OSStatus {status})"
            ));
        }
        // SAFETY: `data` points at `size` bytes owned by the description.
        sets.push(unsafe { std::slice::from_raw_parts(data, size) }.to_vec());
        index += 1;
    }
    Ok((sets, usize::try_from(header).unwrap_or(4)))
}

/// An H.264 encoder for pictures of one size.
pub struct VideoToolboxEncoder {
    session: SessionRef,
    sink: Box<Sink>,
    config: EncoderConfig,
    started: Instant,
    last_stamp: i64,
}

// SAFETY: the session is only used through `&mut self`, and VideoToolbox sessions
// may be used from any one thread at a time.
unsafe impl Send for VideoToolboxEncoder {}

impl std::fmt::Debug for VideoToolboxEncoder {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("VideoToolboxEncoder")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl Drop for VideoToolboxEncoder {
    fn drop(&mut self) {
        // SAFETY: the session is valid; frames still inside are finished, then it
        // is invalidated (no callback runs after that) and its reference released.
        unsafe {
            VTCompressionSessionCompleteFrames(self.session, TIME_INVALID);
            VTCompressionSessionInvalidate(self.session);
            CFRelease(self.session.cast_const());
        }
    }
}

impl VideoToolboxEncoder {
    /// A session for `config`: with the system's low-latency rate control when
    /// it takes it, else the ordinary real-time encoder, else Main profile.
    ///
    /// # Errors
    /// What `VideoToolbox` said when none of the attempts worked.
    pub fn new(config: EncoderConfig) -> Result<Self, String> {
        let mut last = String::from("no attempt was made");
        for (low_latency, attempt) in [(true, config), (false, config), (false, config.fallback())]
        {
            match Self::open(attempt, low_latency) {
                Ok(encoder) => return Ok(encoder),
                Err(reason) => {
                    log::debug!(
                        "H.264 session ({:?}, low latency {low_latency}) failed: {reason}",
                        attempt.profile
                    );
                    last = reason;
                }
            }
        }
        Err(last)
    }

    fn open(config: EncoderConfig, low_latency: bool) -> Result<Self, String> {
        let sink = Box::<Sink>::default();
        let refcon = ptr::from_ref::<Sink>(&sink).cast_mut().cast::<c_void>();
        let specification = low_latency.then(|| {
            // SAFETY: the constant is a valid string for the life of the process.
            let key = unsafe {
                CFString::wrap_under_get_rule(
                    kVTVideoEncoderSpecification_EnableLowLatencyRateControl,
                )
            };
            CFDictionary::<CFString, CFType>::from_CFType_pairs(&[(
                key,
                CFBoolean::from(true).as_CFType(),
            )])
        });
        let to_int = |value: u32| i32::try_from(value).unwrap_or(i32::MAX);
        let mut session: SessionRef = ptr::null_mut();
        // SAFETY: the specification (when any) is valid for the call; the refcon is
        // the sink, kept alive by the result; the out pointer is valid.
        let status = unsafe {
            VTCompressionSessionCreate(
                ptr::null(),
                to_int(config.width),
                to_int(config.height),
                CODEC_H264,
                specification.as_ref().map_or(ptr::null(), |dictionary| {
                    dictionary.as_concrete_TypeRef().cast()
                }),
                ptr::null(),
                ptr::null(),
                output_callback,
                refcon,
                &raw mut session,
            )
        };
        if status != 0 || session.is_null() {
            return Err(format!(
                "VTCompressionSessionCreate failed (OSStatus {status})"
            ));
        }
        let encoder = Self {
            session,
            sink,
            config,
            started: Instant::now(),
            last_stamp: -1,
        };
        encoder.configure(low_latency)?;
        // SAFETY: the session is valid.
        let status = unsafe { VTCompressionSessionPrepareToEncodeFrames(encoder.session) };
        if status != 0 {
            return Err(format!("preparing the session failed (OSStatus {status})"));
        }
        Ok(encoder)
    }

    /// Sets what the encoder is for. The properties every encoder has are
    /// required; the others are skipped when this one does not know them.
    /// `AllowFrameReordering` is required only without low-latency rate
    /// control, which never reorders anyway.
    fn configure(&self, low_latency: bool) -> Result<(), String> {
        let config = &self.config;
        let bytes_per_second = i64::from(config.bitrate) / 8 * 3 / 2;
        let limits = CFArray::<CFType>::from_CFTypes(&[
            CFNumber::from(bytes_per_second).as_CFType(),
            CFNumber::from(1_i64).as_CFType(),
        ]);
        // Held until the end: the property list below borrows them.
        let fps = CFNumber::from(i32::try_from(config.fps).unwrap_or(60));
        let bitrate = CFNumber::from(i32::try_from(config.bitrate).unwrap_or(i32::MAX));
        let key_interval = CFNumber::from(i32::try_from(config.key_interval).unwrap_or(i32::MAX));
        let yes = CFBoolean::from(true);
        let no = CFBoolean::from(false);
        // SAFETY: the constants are valid strings for the life of the process.
        let profile = unsafe {
            match config.profile {
                Profile::High => kVTProfileLevel_H264_High_AutoLevel,
                Profile::Main => kVTProfileLevel_H264_Main_AutoLevel,
            }
        };
        // SAFETY: as above.
        let required: [(CFStringRef, CFTypeRef); 5] = unsafe {
            [
                (kVTCompressionPropertyKey_ProfileLevel, profile.cast()),
                (kVTCompressionPropertyKey_RealTime, yes.as_CFTypeRef()),
                (
                    kVTCompressionPropertyKey_AverageBitRate,
                    bitrate.as_CFTypeRef(),
                ),
                (
                    kVTCompressionPropertyKey_MaxKeyFrameInterval,
                    key_interval.as_CFTypeRef(),
                ),
                (
                    kVTCompressionPropertyKey_ExpectedFrameRate,
                    fps.as_CFTypeRef(),
                ),
            ]
        };
        for (key, value) in required {
            self.set(key, value).map_err(|status| {
                format!("a required encoder property was refused (OSStatus {status})")
            })?;
        }
        // SAFETY: as above.
        let reordering = unsafe { kVTCompressionPropertyKey_AllowFrameReordering };
        if let Err(status) = self.set(reordering, no.as_CFTypeRef())
            && !low_latency
        {
            return Err(format!(
                "B-frames could not be turned off (OSStatus {status})"
            ));
        }
        // The viewers decode BT.601, limited range, like every H.264 stream they get
        // from other servers; tagging it keeps the colours right.
        // SAFETY: as above.
        let optional: [(CFStringRef, CFTypeRef); 5] = unsafe {
            [
                (
                    kVTCompressionPropertyKey_ColorPrimaries,
                    kCVImageBufferColorPrimaries_SMPTE_C.cast(),
                ),
                (
                    kVTCompressionPropertyKey_TransferFunction,
                    kCVImageBufferTransferFunction_ITU_R_709_2.cast(),
                ),
                (
                    kVTCompressionPropertyKey_YCbCrMatrix,
                    kCVImageBufferYCbCrMatrix_ITU_R_601_4.cast(),
                ),
                (
                    kVTCompressionPropertyKey_DataRateLimits,
                    limits.as_CFTypeRef(),
                ),
                (
                    kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality,
                    yes.as_CFTypeRef(),
                ),
            ]
        };
        for (key, value) in optional {
            if let Err(status) = self.set(key, value) {
                log::debug!("an optional encoder property was refused (OSStatus {status})");
            }
        }
        Ok(())
    }

    fn set(&self, key: CFStringRef, value: CFTypeRef) -> Result<(), OsStatus> {
        // SAFETY: the session, key and value are valid for the call.
        let status = unsafe { VTSessionSetProperty(self.session, key, value) };
        if status == 0 { Ok(()) } else { Err(status) }
    }

    /// The next presentation time: the wall clock in milliseconds, strictly increasing.
    fn stamp(&mut self) -> CmTime {
        let now = i64::try_from(self.started.elapsed().as_millis()).unwrap_or(i64::MAX);
        self.last_stamp = now.max(self.last_stamp + 1);
        CmTime {
            value: self.last_stamp,
            timescale: TIMESCALE,
            flags: TIME_FLAGS_VALID,
            epoch: 0,
        }
    }
}

impl Encoder for VideoToolboxEncoder {
    fn encode(&mut self, frame: &Frame, key: bool) -> Result<Vec<u8>, String> {
        let uploaded;
        let buffer: &PixelBuffer = match &frame.data {
            FrameData::Surface(buffer) => buffer,
            FrameData::Cpu { stride, bytes } => {
                uploaded = PixelBuffer::from_bgra(frame.width, frame.height, *stride, bytes)
                    .ok_or("the picture could not be copied for encoding")?;
                &uploaded
            }
        };
        if (buffer.width(), buffer.height()) != (self.config.width, self.config.height) {
            return Err("the picture is not the size the encoder was made for".into());
        }
        let properties = key.then(|| {
            // SAFETY: the constant is a valid string for the life of the process.
            let option =
                unsafe { CFString::wrap_under_get_rule(kVTEncodeFrameOptionKey_ForceKeyFrame) };
            CFDictionary::<CFString, CFType>::from_CFType_pairs(&[(
                option,
                CFBoolean::from(true).as_CFType(),
            )])
        });
        let stamp = self.stamp();
        let mut flags = 0_u32;
        // SAFETY: the session and the buffer are valid; the properties (when any)
        // are valid for the call.
        let status = unsafe {
            VTCompressionSessionEncodeFrame(
                self.session,
                buffer.as_ptr(),
                stamp,
                TIME_INVALID,
                properties.as_ref().map_or(ptr::null(), |dictionary| {
                    dictionary.as_concrete_TypeRef().cast()
                }),
                ptr::null_mut(),
                &raw mut flags,
            )
        };
        if status != 0 {
            return Err(format!("encoding failed (OSStatus {status})"));
        }
        // SAFETY: the session is valid.
        let status = unsafe { VTCompressionSessionCompleteFrames(self.session, TIME_INVALID) };
        if status != 0 {
            return Err(format!("finishing the picture failed (OSStatus {status})"));
        }
        if let Some(reason) = self
            .sink
            .error
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            return Err(reason);
        }
        let units = std::mem::take(
            &mut *self
                .sink
                .units
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        Ok(units.concat())
    }
}
