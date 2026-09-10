//! UU fallback/configuration owner, using this project's platform decoders.
//! Official lifecycle: CF7D30/CF8240/CF87B0/CF8BC0, not a boolean HW->SW retry.

use bytes::Bytes;

use crate::{
    decoder::{DecoderCandidate, NativeVideoDecoder},
    decoder_result::VideoDecodeResult,
    media::VideoCodec,
    video_format::VideoFormatSignature,
};

struct Candidate {
    kind: DecoderCandidate,
    enabled: bool,
}

pub(crate) struct DecoderPool {
    decoder: Option<NativeVideoDecoder>,
    candidates: Vec<Candidate>,
    active: usize,
    codec: VideoCodec,
    format: Option<VideoFormatSignature>,
    width: u32,
    height: u32,
    frame_rate: u32,
    prefer_hardware: bool,
    extra_data: Bytes,
    error_count: u32,
    notification: crate::decoder::platform::DecoderNotification,
    callback_result: Option<VideoDecodeResult>,
    blocked_reason: Option<String>,
    #[cfg(windows)]
    software_slot: Option<std::rc::Rc<crate::decoder::software_slot::SoftwareSlot>>,
    #[cfg(windows)]
    writer: Option<crate::decoder::windows_surface::D3D11SurfaceWriter>,
}

pub(crate) struct DecoderTransition {
    /// A replaced/released decoder invalidates its inflight generation even if
    /// its replacement fails to initialize.
    pub replaced: bool,
    pub result: Option<VideoDecodeResult>,
}

impl DecoderPool {
    pub(crate) fn new(
        decoder: NativeVideoDecoder,
        codec: VideoCodec,
        width: u32,
        height: u32,
        frame_rate: u32,
        prefer_hardware: bool,
        extra_data: Bytes,
    ) -> Self {
        let candidates = DecoderCandidate::available(codec, prefer_hardware)
            .into_iter()
            .map(|kind| Candidate {
                kind,
                enabled: true,
            })
            .collect::<Vec<_>>();
        let active = candidates
            .iter()
            .position(|entry| entry.kind == decoder.candidate())
            .expect("opened decoder belongs to the configured candidate list");
        Self {
            #[cfg(windows)]
            software_slot: decoder.software_slot(),
            #[cfg(windows)]
            writer: decoder.surface_writer(),
            decoder: Some(decoder),
            candidates,
            active,
            codec,
            format: None,
            width,
            height,
            frame_rate,
            prefer_hardware,
            extra_data,
            error_count: 0,
            notification: crate::decoder::platform::DecoderNotification::default(),
            callback_result: None,
            blocked_reason: None,
        }
    }

    pub(crate) fn label(&self) -> &str {
        self.decoder
            .as_ref()
            .map_or("等待可用解码器", NativeVideoDecoder::label)
    }

    pub(crate) fn decoder(&mut self) -> Option<&mut NativeVideoDecoder> {
        self.decoder.as_mut()
    }

    pub(crate) fn is_software(&self) -> bool {
        self.decoder
            .as_ref()
            .is_some_and(NativeVideoDecoder::is_software)
    }

    pub(crate) fn blocked_reason(&self) -> Option<&str> {
        self.blocked_reason.as_deref()
    }

    pub(crate) fn set_notification(
        &mut self,
        notification: crate::decoder::platform::DecoderNotification,
    ) {
        if let Some(decoder) = self.decoder.as_mut() {
            decoder.set_notification(notification.clone());
        }
        self.notification = notification;
    }

    pub(crate) fn callback_failed(&mut self, error: &anyhow::Error) {
        // Output errors change adapter state; the next Decode consumes that
        // state. They do not immediately recreate a decoder or send a PLI.
        self.callback_result = Some(
            if matches!(
                error.downcast_ref::<crate::decoder::platform::DecodeError>(),
                Some(
                    crate::decoder::platform::DecodeError::Unsupported
                        | crate::decoder::platform::DecodeError::HardwareFailure
                )
            ) {
                VideoDecodeResult::Fallback
            } else {
                VideoDecodeResult::RequestKeyframe
            },
        );
    }

    pub(crate) fn callback_result(&self) -> Option<VideoDecodeResult> {
        self.callback_result
    }

    pub(crate) fn prepare(
        &mut self,
        codec: VideoCodec,
        format: Option<VideoFormatSignature>,
        keyframe: bool,
    ) -> DecoderTransition {
        let codec_changed = codec != self.codec;
        let format_changed = format.is_some_and(|next| {
            self.format
                .is_none_or(|current| !same_configuration(current, next))
        });
        if !codec_changed && !format_changed {
            // Cropping origin is decoder output metadata, not one of the six
            // Configure comparison fields in the UU wrapper.
            if format.is_some() {
                self.format = format;
            }
            return DecoderTransition {
                replaced: false,
                result: None,
            };
        }
        if !keyframe {
            return DecoderTransition {
                replaced: false,
                result: Some(VideoDecodeResult::RequestKeyframe),
            };
        }
        if codec_changed {
            self.codec = codec;
            self.candidates = DecoderCandidate::available(codec, self.prefer_hardware)
                .into_iter()
                .map(|kind| Candidate {
                    kind,
                    enabled: true,
                })
                .collect();
            self.active = 0;
            self.format = None;
        }
        self.release();
        let width = format.map_or(self.width, |format| format.visible_width);
        let height = format.map_or(self.height, |format| format.visible_height);
        let initialized = self.select(width, height, format);
        if initialized {
            self.width = width;
            self.height = height;
            self.format = format;
            tracing::info!(
                ?codec,
                ?format,
                decoder = self.label(),
                "UU decoder configuration replaced"
            );
        }
        // Per-frame geometry reconfiguration retains candidate validity; only
        // external Configure or a new codec re-enables the full candidate set.
        DecoderTransition {
            replaced: true,
            result: (!initialized).then_some(VideoDecodeResult::Error),
        }
    }

    pub(crate) fn complete(
        &mut self,
        result: VideoDecodeResult,
        keyframe: bool,
        input_format: Option<VideoFormatSignature>,
    ) -> DecoderTransition {
        if result == VideoDecodeResult::Decoded {
            self.error_count = 0;
        } else if result.counts_towards_fallback(keyframe) {
            self.error_count = self.error_count.saturating_add(1);
        }
        if self.error_count < 5 && result != VideoDecodeResult::Fallback {
            return DecoderTransition {
                replaced: false,
                result: Some(result),
            };
        }
        let enabled_count = self.candidates.iter().filter(|entry| entry.enabled).count();
        let failed = self.candidates[self.active].kind;
        if enabled_count > 1 {
            self.candidates[self.active].enabled = false;
        }
        if let Some(format) = input_format {
            self.width = format.visible_width;
            self.height = format.visible_height;
            if let Some(current) = self.format.as_mut() {
                current.coded_width = format.coded_width;
                current.coded_height = format.coded_height;
                current.visible_width = format.visible_width;
                current.visible_height = format.visible_height;
            }
        }
        tracing::warn!(
            ?failed,
            ?result,
            errors = self.error_count,
            enabled_count,
            width = self.width,
            height = self.height,
            "UU decoder fallback transition"
        );
        self.release();
        let initialized = self.select(self.width, self.height, self.format.or(input_format));
        DecoderTransition {
            replaced: true,
            // CF87B0 initializes immediately but does not retry the failed frame.
            result: Some(if initialized {
                VideoDecodeResult::RequestKeyframe
            } else {
                VideoDecodeResult::Error
            }),
        }
    }

    fn release(&mut self) {
        self.decoder.take();
        self.error_count = 0;
        self.callback_result = None;
    }

    fn select(&mut self, width: u32, height: u32, format: Option<VideoFormatSignature>) -> bool {
        self.error_count = 0;
        self.blocked_reason = None;
        for (index, entry) in self.candidates.iter().enumerate() {
            if !entry.enabled {
                continue;
            }
            // Match actual stream parameters before choosing a backend. In
            // particular H264 4:4:4 belongs to the software candidate, rather
            // than losing its first keyframe to a predictable hardware error.
            #[cfg(windows)]
            if let Some(format) = format {
                let supported = match entry.kind {
                    DecoderCandidate::WindowsD3d11 => {
                        format.chroma_format_idc == 1
                            && format.bit_depth_luma == format.bit_depth_chroma
                            && (format.bit_depth_luma == 8
                                || self.codec == VideoCodec::H265 && format.bit_depth_luma == 10)
                    }
                    DecoderCandidate::PlatformMemory => {
                        self.codec == VideoCodec::H264
                            && matches!(format.chroma_format_idc, 1 | 3)
                            && format.bit_depth_luma == 8
                            && format.bit_depth_chroma == 8
                    }
                };
                if !supported {
                    continue;
                }
            }
            #[cfg(not(windows))]
            let _ = format;
            match NativeVideoDecoder::open_candidate(
                entry.kind,
                self.codec,
                width,
                height,
                self.frame_rate,
                self.extra_data.clone(),
                #[cfg(windows)]
                self.writer.clone(),
                #[cfg(windows)]
                self.software_slot.clone(),
            ) {
                Ok(mut decoder) => {
                    #[cfg(windows)]
                    {
                        self.software_slot = decoder.software_slot();
                    }
                    decoder.set_notification(self.notification.clone());
                    #[cfg(windows)]
                    if let Some(writer) = decoder.surface_writer() {
                        self.writer = Some(writer);
                    }
                    self.active = index;
                    self.decoder = Some(decoder);
                    return true;
                }
                Err(error) => {
                    #[cfg(windows)]
                    if error.is::<crate::decoder::software_slot::SoftwarePlaybackBusy>() {
                        self.blocked_reason = Some(error.to_string());
                    }
                    tracing::warn!(candidate = ?entry.kind, %error,
                        width, height, "decoder candidate rejected current configuration");
                }
            }
        }
        #[cfg(windows)]
        {
            self.software_slot = None;
        }
        false
    }
}

fn same_configuration(a: VideoFormatSignature, b: VideoFormatSignature) -> bool {
    a.chroma_format_idc == b.chroma_format_idc
        && a.bit_depth_luma == b.bit_depth_luma
        && a.coded_width == b.coded_width
        && a.coded_height == b.coded_height
        && a.visible_width == b.visible_width
        && a.visible_height == b.visible_height
}
