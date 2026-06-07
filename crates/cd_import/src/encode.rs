// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Audio extraction and encoding via GStreamer.
//!
//! One pipeline is built per physical track from trusted, typed values —
//! never by interpolating user-supplied metadata into a parse-launch
//! string. The element selection per [`CdEncodingProfile`] is a pure
//! function ([`encoder_element`] / [`required_elements`]) so the encoder
//! configuration and the missing-plugin guard are unit-testable without a
//! drive, a disc, or even GStreamer's plugins installed.

use std::path::PathBuf;

use sustain_domain::CdEncodingProfile;

use crate::error::CdImportResult;

/// One physical track to extract and encode.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EncodeRequest {
    /// Optical device to read from (e.g. `/dev/sr0`).
    pub device_path: PathBuf,
    /// Physical track number to extract.
    pub track: u32,
    /// Encoding profile captured at task preparation.
    pub profile: CdEncodingProfile,
    /// Where to write the encoded file — a staging path on the library
    /// filesystem, on the same filesystem as its eventual destination.
    pub destination: PathBuf,
}

/// Monotonic per-track progress, derived from the pipeline's
/// position/duration query. `percent` only ever moves forward within one
/// track.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EncodeProgress {
    pub percent: u8,
}

/// A single typed property to set on the encoder element.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EncoderProperty {
    Bool(&'static str, bool),
    Int(&'static str, i32),
    /// An enum-typed property set by its GStreamer nick (e.g. `target` =
    /// `"bitrate"`). Set through `set_property_from_str` with a constant
    /// nick — never user input.
    EnumNick(&'static str, &'static str),
}

/// The encoder element for a profile: its factory name plus the typed
/// properties to apply.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EncoderElement {
    pub factory: &'static str,
    pub properties: Vec<EncoderProperty>,
}

/// Common chain elements every profile shares, in pipeline order, before
/// the encoder. The source is `cdparanoiasrc` and the sink is `filesink`.
pub const SOURCE_FACTORY: &str = "cdparanoiasrc";
pub const SINK_FACTORY: &str = "filesink";
const CONVERT_FACTORY: &str = "audioconvert";
const RESAMPLE_FACTORY: &str = "audioresample";

/// The encoder element selection for a profile.
///
/// FLAC uses `flacenc` at its normal lossless settings (compression level
/// is file-size/CPU only and is not a v1 user setting). The MP3 profiles
/// use `lamemp3enc` targeting a constant bitrate of exactly 256 or 320
/// kbps — `target=bitrate`, `cbr=true`, and the explicit `bitrate`.
pub fn encoder_element(profile: CdEncodingProfile) -> EncoderElement {
    match profile {
        CdEncodingProfile::Flac => EncoderElement {
            factory: "flacenc",
            properties: Vec::new(),
        },
        CdEncodingProfile::Mp3Cbr256 | CdEncodingProfile::Mp3Cbr320 => {
            let bitrate = profile
                .mp3_bitrate_kbps()
                .expect("the MP3 profiles always report a bitrate");
            EncoderElement {
                factory: "lamemp3enc",
                properties: vec![
                    EncoderProperty::EnumNick("target", "bitrate"),
                    EncoderProperty::Int("bitrate", bitrate as i32),
                    EncoderProperty::Bool("cbr", true),
                ],
            }
        }
    }
}

/// Every GStreamer element factory a profile's pipeline needs, in pipeline
/// order. Used both to build the pipeline and to verify availability before
/// an import is offered.
pub fn required_elements(profile: CdEncodingProfile) -> Vec<&'static str> {
    vec![
        SOURCE_FACTORY,
        CONVERT_FACTORY,
        RESAMPLE_FACTORY,
        encoder_element(profile).factory,
        SINK_FACTORY,
    ]
}

/// The first required element that `is_available` reports missing, if any.
/// Factored out so the missing-plugin path is testable with an injected
/// predicate, independent of which plugins happen to be installed.
pub fn first_missing_element(
    profile: CdEncodingProfile,
    is_available: impl Fn(&str) -> bool,
) -> Option<&'static str> {
    required_elements(profile)
        .into_iter()
        .find(|name| !is_available(name))
}

/// Extracts and encodes physical CD tracks. The production implementation
/// is [`GStreamerCdEncoder`]; tests inject a fake.
pub trait CdEncoder: Send + Sync {
    /// Verify every GStreamer element the profile needs is installed, so the
    /// UI can refuse to start an import with a precise message rather than
    /// failing mid-rip.
    fn ensure_available(&self, profile: CdEncodingProfile) -> CdImportResult<()>;

    /// Extract and encode one physical track to `request.destination`,
    /// reporting monotonic progress and polling `cancelled` so a long rip
    /// stops responsively. Returns once the file is fully written, or with
    /// an error / [`CdImportError::Cancelled`](crate::CdImportError::Cancelled).
    fn encode_track(
        &self,
        request: &EncodeRequest,
        progress: &mut dyn FnMut(EncodeProgress),
        cancelled: &dyn Fn() -> bool,
    ) -> CdImportResult<()>;
}

pub use gst_backend::GStreamerCdEncoder;

mod gst_backend {
    use std::path::Path;
    use std::time::Duration;

    use gst::prelude::*;
    use gstreamer as gst;

    use sustain_domain::CdEncodingProfile;

    use super::{
        CONVERT_FACTORY, CdEncoder, EncodeProgress, EncodeRequest, EncoderProperty,
        RESAMPLE_FACTORY, SINK_FACTORY, SOURCE_FACTORY, encoder_element, first_missing_element,
    };
    use crate::error::{CdImportError, CdImportResult};

    /// `cdparanoiasrc`'s full-paranoia mode nick. Set as a trusted constant,
    /// not user input.
    const PARANOIA_MODE_FULL: &str = "full";

    /// How often the encode loop wakes to pump the bus, sample progress, and
    /// check for cancellation.
    const POLL_INTERVAL: Duration = Duration::from_millis(100);

    /// GStreamer-backed encoder. Zero-sized: every pipeline is built on the
    /// calling worker thread, so the encoder itself is trivially `Send`.
    #[derive(Clone, Copy, Debug, Default)]
    pub struct GStreamerCdEncoder;

    impl GStreamerCdEncoder {
        pub fn new() -> Self {
            Self
        }

        fn make(factory: &str) -> CdImportResult<gst::Element> {
            gst::ElementFactory::make(factory)
                .build()
                .map_err(|_| CdImportError::MissingGstElement(factory.to_owned()))
        }
    }

    impl CdEncoder for GStreamerCdEncoder {
        fn ensure_available(&self, profile: CdEncodingProfile) -> CdImportResult<()> {
            gst::init().map_err(|_| CdImportError::GstInitFailed)?;
            if let Some(missing) =
                first_missing_element(profile, |name| gst::ElementFactory::find(name).is_some())
            {
                return Err(CdImportError::MissingGstElement(missing.to_owned()));
            }
            Ok(())
        }

        fn encode_track(
            &self,
            request: &EncodeRequest,
            progress: &mut dyn FnMut(EncodeProgress),
            cancelled: &dyn Fn() -> bool,
        ) -> CdImportResult<()> {
            gst::init().map_err(|_| CdImportError::GstInitFailed)?;

            let pipeline = gst::Pipeline::new();
            let source = Self::make(SOURCE_FACTORY)?;
            source.set_property("device", path_to_gst_string(&request.device_path)?);
            // `cdparanoiasrc`'s `track` is a `guint` (range 1–99); set it with
            // the `u32` track number directly. Passing an `i32` here makes
            // GObject reject the value type and panics the worker thread.
            source.set_property("track", request.track);
            // A trusted constant nick, never user input.
            source.set_property_from_str("paranoia-mode", PARANOIA_MODE_FULL);

            let convert = Self::make(CONVERT_FACTORY)?;
            let resample = Self::make(RESAMPLE_FACTORY)?;

            let element = encoder_element(request.profile);
            let encoder = Self::make(element.factory)?;
            apply_properties(&encoder, &element.properties);

            let sink = Self::make(SINK_FACTORY)?;
            sink.set_property("location", path_to_gst_string(&request.destination)?);

            let elements = [&source, &convert, &resample, &encoder, &sink];
            pipeline
                .add_many(elements)
                .map_err(|_| CdImportError::PipelineBuildFailed)?;
            gst::Element::link_many(elements).map_err(|_| CdImportError::PipelineBuildFailed)?;

            let outcome = run_pipeline(&pipeline, progress, cancelled);
            // Always tear the pipeline down, success or failure, so the drive
            // and bus are released before the next track.
            let _ = pipeline.set_state(gst::State::Null);
            outcome
        }
    }

    fn run_pipeline(
        pipeline: &gst::Pipeline,
        progress: &mut dyn FnMut(EncodeProgress),
        cancelled: &dyn Fn() -> bool,
    ) -> CdImportResult<()> {
        let bus = pipeline.bus().ok_or(CdImportError::PipelineBuildFailed)?;
        pipeline
            .set_state(gst::State::Playing)
            .map_err(|_| CdImportError::PipelineBuildFailed)?;

        let mut highest_percent = 0u8;
        loop {
            if cancelled() {
                return Err(CdImportError::Cancelled);
            }

            if let Some(message) = bus.timed_pop(gst::ClockTime::from_mseconds(
                POLL_INTERVAL.as_millis() as u64,
            )) {
                match message.view() {
                    gst::MessageView::Eos(_) => return Ok(()),
                    gst::MessageView::Error(error) => {
                        return Err(CdImportError::EncodeFailed(error.error().to_string()));
                    }
                    _ => {}
                }
            }

            if let Some(percent) = sample_progress(pipeline) {
                // Progress is clamped to never move backward within a track.
                if percent > highest_percent {
                    highest_percent = percent;
                    progress(EncodeProgress {
                        percent: highest_percent,
                    });
                }
            }
        }
    }

    fn sample_progress(pipeline: &gst::Pipeline) -> Option<u8> {
        let position = pipeline.query_position::<gst::ClockTime>()?;
        let duration = pipeline.query_duration::<gst::ClockTime>()?;
        let duration_ns = duration.nseconds();
        if duration_ns == 0 {
            return None;
        }
        let ratio = position.nseconds().min(duration_ns) as u128 * 100 / duration_ns as u128;
        Some(ratio.min(100) as u8)
    }

    fn apply_properties(element: &gst::Element, properties: &[EncoderProperty]) {
        for property in properties {
            match *property {
                EncoderProperty::Bool(name, value) => element.set_property(name, value),
                EncoderProperty::Int(name, value) => element.set_property(name, value),
                EncoderProperty::EnumNick(name, nick) => {
                    element.set_property_from_str(name, nick);
                }
            }
        }
    }

    fn path_to_gst_string(path: &Path) -> CdImportResult<String> {
        path.to_str()
            .map(ToOwned::to_owned)
            .ok_or(CdImportError::PipelineBuildFailed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flac_profile_uses_flacenc_with_no_extra_properties() {
        let element = encoder_element(CdEncodingProfile::Flac);
        assert_eq!(element.factory, "flacenc");
        assert!(element.properties.is_empty());
    }

    #[test]
    fn mp3_256_profile_targets_constant_256_kbps() {
        let element = encoder_element(CdEncodingProfile::Mp3Cbr256);
        assert_eq!(element.factory, "lamemp3enc");
        assert_eq!(
            element.properties,
            vec![
                EncoderProperty::EnumNick("target", "bitrate"),
                EncoderProperty::Int("bitrate", 256),
                EncoderProperty::Bool("cbr", true),
            ]
        );
    }

    #[test]
    fn mp3_320_profile_targets_constant_320_kbps() {
        let element = encoder_element(CdEncodingProfile::Mp3Cbr320);
        assert_eq!(element.factory, "lamemp3enc");
        assert_eq!(
            element.properties,
            vec![
                EncoderProperty::EnumNick("target", "bitrate"),
                EncoderProperty::Int("bitrate", 320),
                EncoderProperty::Bool("cbr", true),
            ]
        );
    }

    #[test]
    fn required_elements_cover_the_full_chain_per_profile() {
        assert_eq!(
            required_elements(CdEncodingProfile::Flac),
            vec![
                "cdparanoiasrc",
                "audioconvert",
                "audioresample",
                "flacenc",
                "filesink",
            ]
        );
        assert_eq!(
            required_elements(CdEncodingProfile::Mp3Cbr320),
            vec![
                "cdparanoiasrc",
                "audioconvert",
                "audioresample",
                "lamemp3enc",
                "filesink",
            ]
        );
    }

    #[test]
    fn cdparanoiasrc_accepts_the_u32_track_and_full_paranoia_mode() {
        // Regression guard for the worker-crashing property mismatch:
        // `cdparanoiasrc.track` is a `guint`, so setting it with an `i32`
        // makes GObject reject the value and panics the rip thread (surfacing
        // only as a generic "could not be imported"). Build the real element
        // and set the properties exactly as the encoder does. Skips cleanly
        // where the cdparanoia plugin is not installed (e.g. minimal CI).
        use gst::prelude::*;
        use gstreamer as gst;

        if gst::init().is_err() {
            return;
        }
        let Ok(source) = gst::ElementFactory::make("cdparanoiasrc").build() else {
            return;
        };
        // Must not panic: the `u32` track number and the `full` flag nick.
        source.set_property("track", 7u32);
        source.set_property_from_str("paranoia-mode", "full");
        assert_eq!(source.property::<u32>("track"), 7);
    }

    #[test]
    fn missing_plugin_is_reported_precisely() {
        // Everything available -> nothing missing.
        assert_eq!(
            first_missing_element(CdEncodingProfile::Flac, |_| true),
            None
        );
        // The MP3 encoder is the missing one.
        assert_eq!(
            first_missing_element(CdEncodingProfile::Mp3Cbr256, |name| name != "lamemp3enc"),
            Some("lamemp3enc")
        );
        // A missing source aborts before the encoder is even considered.
        assert_eq!(
            first_missing_element(CdEncodingProfile::Flac, |name| name != "cdparanoiasrc"),
            Some("cdparanoiasrc")
        );
    }
}
