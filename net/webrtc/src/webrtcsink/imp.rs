// SPDX-License-Identifier: MPL-2.0

use crate::utils::{cleanup_codec_caps, is_raw_caps, make_element, Codec, Codecs, NavigationEvent};
use anyhow::Context;
use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_rtp::prelude::*;
use gst_utils::StreamProducer;
use gst_video::subclass::prelude::*;
use gst_webrtc::{WebRTCDataChannel, WebRTCICETransportPolicy};

use futures::prelude::*;

use anyhow::{anyhow, Error};
use gst::glib::once_cell::sync::Lazy;
use std::collections::HashMap;

use std::ops::Mul;
use std::sync::{mpsc, Arc, Condvar, Mutex};

use super::homegrown_cc::CongestionController;
use super::{WebRTCSinkCongestionControl, WebRTCSinkError, WebRTCSinkMitigationMode};
use crate::aws_kvs_signaller::AwsKvsSignaller;
use crate::livekit_signaller::LiveKitSignaller;
use crate::signaller::{prelude::*, Signallable, Signaller, WebRTCSignallerRole};
use crate::whip_signaller::WhipSignaller;
use crate::RUNTIME;
use std::collections::{BTreeMap, HashSet};

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "webrtcsink",
        gst::DebugColorFlags::empty(),
        Some("WebRTC sink"),
    )
});

const CUDA_MEMORY_FEATURE: &str = "memory:CUDAMemory";
const GL_MEMORY_FEATURE: &str = "memory:GLMemory";
const NVMM_MEMORY_FEATURE: &str = "memory:NVMM";

const RTP_TWCC_URI: &str =
    "http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01";

const DEFAULT_STUN_SERVER: Option<&str> = Some("stun://stun.l.google.com:19302");
const DEFAULT_MIN_BITRATE: u32 = 1000;

/* I have found higher values to cause packet loss *somewhere* in
 * my local network, possibly related to chrome's pretty low UDP
 * buffer sizes */
const DEFAULT_MAX_BITRATE: u32 = 8192000;
const DEFAULT_CONGESTION_CONTROL: WebRTCSinkCongestionControl =
    WebRTCSinkCongestionControl::GoogleCongestionControl;
const DEFAULT_DO_FEC: bool = true;
const DEFAULT_DO_RETRANSMISSION: bool = true;
const DEFAULT_ENABLE_DATA_CHANNEL_NAVIGATION: bool = false;
const DEFAULT_ICE_TRANSPORT_POLICY: WebRTCICETransportPolicy = WebRTCICETransportPolicy::All;
const DEFAULT_START_BITRATE: u32 = 2048000;
/* Start adding some FEC when the bitrate > 2Mbps as we found experimentally
 * that it is not worth it below that threshold */
const DO_FEC_THRESHOLD: u32 = 2000000;

#[derive(Debug, Clone, Copy)]
struct CCInfo {
    heuristic: WebRTCSinkCongestionControl,
    min_bitrate: u32,
    max_bitrate: u32,
    start_bitrate: u32,
}

/// User configuration
#[derive(Clone)]
struct Settings {
    video_caps: gst::Caps,
    audio_caps: gst::Caps,
    turn_servers: gst::Array,
    stun_server: Option<String>,
    cc_info: CCInfo,
    do_fec: bool,
    do_retransmission: bool,
    enable_data_channel_navigation: bool,
    meta: Option<gst::Structure>,
    ice_transport_policy: WebRTCICETransportPolicy,
    signaller: Signallable,
}

/// Type of discovery, used to differentiate between initial discovery
/// and discovery initiated by client offer
#[derive(Debug, Clone, PartialEq, Eq)]
enum DiscoveryType {
    /// Initial discovery of our input streams
    Initial,
    /// Discovery to select a specific codec as requested by the remote peer
    CodecSelection,
}

#[derive(Debug, Clone)]
struct DiscoveryInfo {
    id: String,
    type_: DiscoveryType,
    caps: gst::Caps,
    srcs: Arc<Mutex<Vec<gst_app::AppSrc>>>,
}

impl DiscoveryInfo {
    fn new(type_: DiscoveryType, caps: gst::Caps) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            type_,
            caps,
            srcs: Default::default(),
        }
    }

    fn srcs(&self) -> Vec<gst_app::AppSrc> {
        self.srcs.lock().unwrap().clone()
    }

    fn create_src(&self) -> gst_app::AppSrc {
        let src = gst_app::AppSrc::builder()
            .caps(&self.caps)
            .format(gst::Format::Time)
            .build();

        self.srcs.lock().unwrap().push(src.clone());

        src
    }
}

/// Wrapper around our sink pads
#[derive(Debug, Clone)]
struct InputStream {
    sink_pad: gst::GhostPad,
    producer: Option<StreamProducer>,
    /// The (fixed) caps coming in
    in_caps: Option<gst::Caps>,
    /// The caps we will offer, as a set of fixed structures
    out_caps: Option<gst::Caps>,
    /// Pace input data
    clocksync: Option<gst::Element>,
    /// The serial number picked for this stream
    serial: u32,
    /// Whether the input stream is video or not
    is_video: bool,
    /// Information about currently running codec discoveries
    discoveries: Vec<DiscoveryInfo>,
}

/// Wrapper around webrtcbin pads
#[derive(Clone, Debug)]
struct WebRTCPad {
    pad: gst::Pad,
    /// The (fixed) caps of the corresponding input stream
    in_caps: gst::Caps,
    /// The m= line index in the SDP
    media_idx: u32,
    ssrc: u32,
    /// The name of the corresponding InputStream's sink_pad.
    /// When None, the pad was only created to mark its transceiver
    /// as inactive (in the case where we answer an offer).
    stream_name: Option<String>,
    /// The payload selected in the answer, None at first
    payload: Option<i32>,
}

/// Wrapper around GStreamer encoder element, keeps track of factory
/// name in order to provide a unified set / get bitrate API, also
/// tracks a raw capsfilter used to resize / decimate the input video
/// stream according to the bitrate, thresholds hardcoded for now
pub struct VideoEncoder {
    factory_name: String,
    codec_name: String,
    element: gst::Element,
    filter: gst::Element,
    halved_framerate: gst::Fraction,
    video_info: gst_video::VideoInfo,
    session_id: String,
    mitigation_mode: WebRTCSinkMitigationMode,
    pub transceiver: gst_webrtc::WebRTCRTPTransceiver,
}

struct Session {
    id: String,

    pipeline: gst::Pipeline,
    webrtcbin: gst::Element,
    rtprtxsend: Option<gst::Element>,
    webrtc_pads: HashMap<u32, WebRTCPad>,
    peer_id: String,
    encoders: Vec<VideoEncoder>,

    // Our Homegrown controller (if cc_info.heuristic == Homegrown)
    congestion_controller: Option<CongestionController>,
    // Our BandwidthEstimator (if cc_info.heuristic == GoogleCongestionControl)
    rtpgccbwe: Option<gst::Element>,

    sdp: Option<gst_sdp::SDPMessage>,
    stats: gst::Structure,

    cc_info: CCInfo,

    links: HashMap<u32, gst_utils::ConsumptionLink>,
    stats_sigid: Option<glib::SignalHandlerId>,

    // When not None, constructed from offer SDP
    codecs: Option<BTreeMap<i32, Codec>>,

    stats_collection_handle: Option<tokio::task::JoinHandle<()>>,
}

#[derive(Debug, PartialEq, Eq, Copy, Clone)]
enum SignallerState {
    Started,
    Stopped,
}

// Used to ensure signal are disconnected when a new signaller is is
#[allow(dead_code)]
struct SignallerSignals {
    error: glib::SignalHandlerId,
    request_meta: glib::SignalHandlerId,
    session_requested: glib::SignalHandlerId,
    session_ended: glib::SignalHandlerId,
    session_description: glib::SignalHandlerId,
    handle_ice: glib::SignalHandlerId,
    shutdown: glib::SignalHandlerId,
}

/* Our internal state */
struct State {
    signaller_state: SignallerState,
    sessions: HashMap<String, Session>,
    codecs: BTreeMap<i32, Codec>,
    /// Used to abort codec discovery
    codecs_abort_handles: Vec<futures::future::AbortHandle>,
    /// Used to wait for the discovery task to fully stop
    codecs_done_receivers: Vec<futures::channel::oneshot::Receiver<()>>,
    /// Used to determine whether we can start the signaller when going to Playing,
    /// or whether we should wait
    codec_discovery_done: bool,
    audio_serial: u32,
    video_serial: u32,
    streams: HashMap<String, InputStream>,
    navigation_handler: Option<NavigationEventHandler>,
    mids: HashMap<String, String>,
    signaller_signals: Option<SignallerSignals>,
    finalizing_sessions: Arc<(Mutex<HashSet<String>>, Condvar)>,
}

fn create_navigation_event(sink: &super::BaseWebRTCSink, msg: &str) {
    let event: Result<NavigationEvent, _> = serde_json::from_str(msg);

    if let Ok(event) = event {
        gst::log!(CAT, obj: sink, "Processing navigation event: {:?}", event);

        if let Some(mid) = event.mid {
            let this = sink.imp();

            let state = this.state.lock().unwrap();
            if let Some(stream_name) = state.mids.get(&mid) {
                if let Some(stream) = state.streams.get(stream_name) {
                    let event = gst::event::Navigation::new(event.event.structure());

                    if !stream.sink_pad.push_event(event.clone()) {
                        gst::info!(CAT, "Could not send event: {:?}", event);
                    }
                }
            }
        } else {
            let this = sink.imp();

            let state = this.state.lock().unwrap();
            let event = gst::event::Navigation::new(event.event.structure());
            state.streams.iter().for_each(|(_, stream)| {
                if stream.sink_pad.name().starts_with("video_") {
                    gst::log!(CAT, "Navigating to: {:?}", event);
                    if !stream.sink_pad.push_event(event.clone()) {
                        gst::info!(CAT, "Could not send event: {:?}", event);
                    }
                }
            });
        }
    } else {
        gst::error!(CAT, "Invalid navigation event: {:?}", msg);
    }
}

/// Simple utility for tearing down a pipeline cleanly
struct PipelineWrapper(gst::Pipeline);

// Structure to generate GstNavigation event from a WebRTCDataChannel
// This is simply used to hold references to the inner items.
#[derive(Debug)]
struct NavigationEventHandler((glib::SignalHandlerId, WebRTCDataChannel));

/// Our instance structure
#[derive(Default)]
pub struct BaseWebRTCSink {
    state: Mutex<State>,
    settings: Mutex<Settings>,
}

impl Default for Settings {
    fn default() -> Self {
        let signaller = Signaller::new(WebRTCSignallerRole::Producer);

        Self {
            video_caps: Codecs::video_codecs()
                .into_iter()
                .flat_map(|codec| codec.caps.iter().map(|s| s.to_owned()).collect::<Vec<_>>())
                .collect::<gst::Caps>(),
            audio_caps: Codecs::audio_codecs()
                .into_iter()
                .flat_map(|codec| codec.caps.iter().map(|s| s.to_owned()).collect::<Vec<_>>())
                .collect::<gst::Caps>(),
            stun_server: DEFAULT_STUN_SERVER.map(String::from),
            turn_servers: gst::Array::new(Vec::new() as Vec<glib::SendValue>),
            cc_info: CCInfo {
                heuristic: WebRTCSinkCongestionControl::GoogleCongestionControl,
                min_bitrate: DEFAULT_MIN_BITRATE,
                max_bitrate: DEFAULT_MAX_BITRATE,
                start_bitrate: DEFAULT_START_BITRATE,
            },
            do_fec: DEFAULT_DO_FEC,
            do_retransmission: DEFAULT_DO_RETRANSMISSION,
            enable_data_channel_navigation: DEFAULT_ENABLE_DATA_CHANNEL_NAVIGATION,
            meta: None,
            ice_transport_policy: DEFAULT_ICE_TRANSPORT_POLICY,
            signaller: signaller.upcast(),
        }
    }
}

impl Default for State {
    fn default() -> Self {
        Self {
            signaller_state: SignallerState::Stopped,
            sessions: HashMap::new(),
            codecs: BTreeMap::new(),
            codecs_abort_handles: Vec::new(),
            codecs_done_receivers: Vec::new(),
            codec_discovery_done: false,
            audio_serial: 0,
            video_serial: 0,
            streams: HashMap::new(),
            navigation_handler: None,
            mids: HashMap::new(),
            signaller_signals: Default::default(),
            finalizing_sessions: Arc::new((Mutex::new(HashSet::new()), Condvar::new())),
        }
    }
}

fn make_converter_for_video_caps(caps: &gst::Caps, codec: &Codec) -> Result<gst::Element, Error> {
    assert!(caps.is_fixed());

    let video_info = gst_video::VideoInfo::from_caps(caps)?;

    let ret = gst::Bin::default();

    let (head, mut tail) = {
        if let Some(feature) = caps.features(0) {
            if feature.contains(NVMM_MEMORY_FEATURE)
                // NVIDIA V4L2 encoders require NVMM memory as input and that requires using the
                // corresponding converter
                || codec
                    .encoder_factory()
                    .map_or(false, |factory| factory.name().starts_with("nvv4l2"))
            {
                let queue = make_element("queue", None)?;
                let nvconvert = if let Ok(nvconvert) = make_element("nvvideoconvert", None) {
                    nvconvert.set_property_from_str("compute-hw", "Default");
                    nvconvert.set_property_from_str("nvbuf-memory-type", "nvbuf-mem-default");
                    nvconvert
                } else {
                    make_element("nvvidconv", None)?
                };

                ret.add_many([&queue, &nvconvert])?;
                gst::Element::link_many([&queue, &nvconvert])?;

                (queue, nvconvert)
            } else if feature.contains(CUDA_MEMORY_FEATURE) {
                if let Some(convert_factory) = gst::ElementFactory::find("cudaconvert") {
                    let cudaupload = make_element("cudaupload", None)?;
                    let cudaconvert = convert_factory.create().build()?;
                    let cudascale = make_element("cudascale", None)?;

                    ret.add_many([&cudaupload, &cudaconvert, &cudascale])?;
                    gst::Element::link_many([&cudaupload, &cudaconvert, &cudascale])?;

                    (cudaupload, cudascale)
                } else {
                    let cudadownload = make_element("cudadownload", None)?;
                    let convert = make_element("videoconvert", None)?;
                    let scale = make_element("videoscale", None)?;

                    gst::warning!(
                        CAT,
                        "No cudaconvert factory available, falling back to software"
                    );

                    ret.add_many([&cudadownload, &convert, &scale])?;
                    gst::Element::link_many([&cudadownload, &convert, &scale])?;

                    (cudadownload, scale)
                }
            } else if feature.contains(GL_MEMORY_FEATURE) {
                let glupload = make_element("glupload", None)?;
                let glconvert = make_element("glcolorconvert", None)?;
                let glscale = make_element("glcolorscale", None)?;

                ret.add_many([&glupload, &glconvert, &glscale])?;
                gst::Element::link_many([&glupload, &glconvert, &glscale])?;

                (glupload, glscale)
            } else {
                let convert = make_element("videoconvert", None)?;
                let scale = make_element("videoscale", None)?;

                ret.add_many([&convert, &scale])?;
                gst::Element::link_many([&convert, &scale])?;

                (convert, scale)
            }
        } else {
            let convert = make_element("videoconvert", None)?;
            let scale = make_element("videoscale", None)?;

            ret.add_many([&convert, &scale])?;
            gst::Element::link_many([&convert, &scale])?;

            (convert, scale)
        }
    };

    ret.add_pad(&gst::GhostPad::with_target(&head.static_pad("sink").unwrap()).unwrap())
        .unwrap();

    if video_info.fps().numer() != 0 {
        let vrate = make_element("videorate", None)?;
        vrate.set_property("drop-only", true);
        vrate.set_property("skip-to-first", true);

        ret.add(&vrate)?;
        tail.link(&vrate)?;
        tail = vrate;
    }

    ret.add_pad(&gst::GhostPad::with_target(&tail.static_pad("src").unwrap()).unwrap())
        .unwrap();

    Ok(ret.upcast())
}

/// Add a pad probe to convert force-keyunit events to the custom action signal based NVIDIA
/// encoder API.
fn add_nv4l2enc_force_keyunit_workaround(enc: &gst::Element) {
    use std::sync::atomic::{self, AtomicBool};

    let srcpad = enc.static_pad("src").unwrap();
    let saw_buffer = AtomicBool::new(false);
    srcpad
        .add_probe(
            gst::PadProbeType::BUFFER
                | gst::PadProbeType::BUFFER_LIST
                | gst::PadProbeType::EVENT_UPSTREAM,
            move |pad, info| {
                match info.data {
                    Some(gst::PadProbeData::Buffer(..))
                    | Some(gst::PadProbeData::BufferList(..)) => {
                        saw_buffer.store(true, atomic::Ordering::SeqCst);
                    }
                    Some(gst::PadProbeData::Event(ref ev))
                        if gst_video::ForceKeyUnitEvent::is(ev)
                            && saw_buffer.load(atomic::Ordering::SeqCst) =>
                    {
                        let enc = pad.parent().unwrap();
                        enc.emit_by_name::<()>("force-IDR", &[]);
                    }
                    _ => {}
                }

                gst::PadProbeReturn::Ok
            },
        )
        .unwrap();
}

/// Default configuration for known encoders, can be disabled
/// by returning True from an encoder-setup handler.
fn configure_encoder(enc: &gst::Element, start_bitrate: u32) {
    if let Some(factory) = enc.factory() {
        match factory.name().as_str() {
            "vp8enc" | "vp9enc" => {
                enc.set_property("deadline", 1i64);
                enc.set_property("target-bitrate", start_bitrate as i32);
                enc.set_property("cpu-used", -16i32);
                enc.set_property("keyframe-max-dist", 2000i32);
                enc.set_property_from_str("keyframe-mode", "disabled");
                enc.set_property_from_str("end-usage", "cbr");
                enc.set_property("buffer-initial-size", 100i32);
                enc.set_property("buffer-optimal-size", 120i32);
                enc.set_property("buffer-size", 150i32);
                enc.set_property("max-intra-bitrate", 250i32);
                enc.set_property_from_str("error-resilient", "default");
                enc.set_property("lag-in-frames", 0i32);
            }
            "x264enc" => {
                enc.set_property("bitrate", start_bitrate / 1000);
                enc.set_property_from_str("tune", "zerolatency");
                enc.set_property_from_str("speed-preset", "ultrafast");
                enc.set_property("threads", 4u32);
                enc.set_property("key-int-max", 2560u32);
                enc.set_property("b-adapt", false);
                enc.set_property("vbv-buf-capacity", 120u32);
            }
            "nvh264enc" => {
                enc.set_property("bitrate", start_bitrate / 1000);
                enc.set_property("gop-size", 2560i32);
                enc.set_property_from_str("rc-mode", "cbr-ld-hq");
                enc.set_property("zerolatency", true);
            }
            "vaapih264enc" | "vaapivp8enc" => {
                enc.set_property("bitrate", start_bitrate / 1000);
                enc.set_property("keyframe-period", 2560u32);
                enc.set_property_from_str("rate-control", "cbr");
            }
            "nvv4l2h264enc" => {
                enc.set_property("bitrate", start_bitrate);
                enc.set_property_from_str("preset-level", "UltraFastPreset");
                enc.set_property("maxperf-enable", true);
                enc.set_property("insert-vui", true);
                enc.set_property("idrinterval", 256u32);
                enc.set_property("insert-sps-pps", true);
                enc.set_property("insert-aud", true);
                enc.set_property_from_str("control-rate", "constant_bitrate");
                add_nv4l2enc_force_keyunit_workaround(enc);
            }
            "nvv4l2vp8enc" | "nvv4l2vp9enc" => {
                enc.set_property("bitrate", start_bitrate);
                enc.set_property_from_str("preset-level", "UltraFastPreset");
                enc.set_property("maxperf-enable", true);
                enc.set_property("idrinterval", 256u32);
                enc.set_property_from_str("control-rate", "constant_bitrate");
                add_nv4l2enc_force_keyunit_workaround(enc);
            }
            _ => (),
        }
    }
}

/// Set of elements used in an EncodingChain
struct EncodingChain {
    raw_filter: Option<gst::Element>,
    encoder: Option<gst::Element>,
    pay_filter: gst::Element,
}

struct EncodingChainBuilder {
    /// Caps of the input chain
    input_caps: gst::Caps,
    //// Caps expected after the payloader
    output_caps: gst::Caps,
    ///  The Codec representing wanted encoding
    codec: Codec,
    /// The SSRC to use for the RTP stream if any
    /// Filter element between the encoder and the payloader.
    encoded_filter: Option<gst::Element>,
    ssrc: Option<u32>,
    /// The TWCC ID to use for payloaded stream
    twcc: Option<u32>,
}

impl EncodingChainBuilder {
    fn new(
        input_caps: &gst::Caps,
        output_caps: &gst::Caps,
        codec: &Codec,
        encoded_filter: Option<gst::Element>,
    ) -> Self {
        Self {
            input_caps: input_caps.clone(),
            output_caps: output_caps.clone(),
            codec: codec.clone(),
            encoded_filter,
            ssrc: None,
            twcc: None,
        }
    }

    fn ssrc(mut self, ssrc: u32) -> Self {
        self.ssrc = Some(ssrc);
        self
    }

    fn twcc(mut self, twcc: u32) -> Self {
        self.twcc = Some(twcc);
        self
    }

    fn build(self, pipeline: &gst::Pipeline, src: &gst::Element) -> Result<EncodingChain, Error> {
        gst::trace!(
            CAT,
            obj: pipeline,
            "Setting up encoding, input caps: {input_caps}, \
                    output caps: {output_caps}, codec: {codec:?}, twcc: {twcc:?}",
            input_caps = self.input_caps,
            output_caps = self.output_caps,
            codec = self.codec,
            twcc = self.twcc,
        );

        let needs_encoding = is_raw_caps(&self.input_caps);
        let mut elements: Vec<gst::Element> = Vec::new();

        let (raw_filter, encoder) = if needs_encoding {
            elements.push(match self.codec.is_video() {
                true => make_converter_for_video_caps(&self.input_caps, &self.codec)?.upcast(),
                false => {
                    gst::parse_bin_from_description("audioresample ! audioconvert", true)?.upcast()
                }
            });

            let raw_filter = self.codec.raw_converter_filter()?;
            elements.push(raw_filter.clone());

            let encoder = self
                .codec
                .build_encoder()
                .expect("We should always have an encoder for negotiated codecs")?;
            elements.push(encoder.clone());
            elements.push(make_element("capsfilter", None)?);

            (Some(raw_filter), Some(encoder))
        } else {
            (None, None)
        };

        if let Some(parser) = self.codec.build_parser()? {
            elements.push(parser);
        }

        // Only force the profile when output caps were not specified, either
        // through input caps or because we are answering an offer
        let force_profile = self.output_caps.is_any() && needs_encoding;
        elements.push(
            gst::ElementFactory::make("capsfilter")
                .property("caps", self.codec.parser_caps(force_profile))
                .build()
                .with_context(|| "Failed to make element capsfilter")?,
        );

        if let Some(ref encoded_filter) = self.encoded_filter {
            elements.push(encoded_filter.clone());
        }

        let pay = self
            .codec
            .build_payloader(
                self.codec
                    .payload()
                    .expect("Negotiated codec should always have pt set") as u32,
            )
            .expect("Payloaders should always have been set in the CodecInfo we handle");

        if let Some(ssrc) = self.ssrc {
            pay.set_property("ssrc", ssrc);
        }

        /* We only enforce TWCC in the offer caps, once a remote description
         * has been set it will get automatically negotiated. This is necessary
         * because the implementor in Firefox had apparently not understood the
         * concept of *transport-wide* congestion control, and firefox doesn't
         * provide feedback for audio packets.
         */
        if let Some(idx) = self.twcc {
            let twcc_extension =
                gst_rtp::RTPHeaderExtension::create_from_uri(RTP_TWCC_URI).unwrap();
            twcc_extension.set_id(idx);
            pay.emit_by_name::<()>("add-extension", &[&twcc_extension]);
        }
        elements.push(pay);

        let pay_filter = gst::ElementFactory::make("capsfilter")
            .property("caps", self.output_caps)
            .build()
            .with_context(|| "Failed to make payloader")?;
        elements.push(pay_filter.clone());

        for element in &elements {
            pipeline.add(element).unwrap();
        }

        elements.insert(0, src.clone());
        gst::Element::link_many(elements.iter().collect::<Vec<&gst::Element>>().as_slice())
            .with_context(|| "Linking encoding elements")?;

        Ok(EncodingChain {
            raw_filter,
            encoder,
            pay_filter,
        })
    }
}

impl VideoEncoder {
    fn new(
        encoding_elements: &EncodingChain,
        video_info: gst_video::VideoInfo,
        session_id: &str,
        codec_name: &str,
        transceiver: gst_webrtc::WebRTCRTPTransceiver,
    ) -> Option<Self> {
        let halved_framerate = video_info.fps().mul(gst::Fraction::new(1, 2));
        Some(Self {
            factory_name: encoding_elements
                .encoder
                .as_ref()?
                .factory()
                .unwrap()
                .name()
                .into(),
            codec_name: codec_name.to_string(),
            element: encoding_elements.encoder.as_ref()?.clone(),
            filter: encoding_elements.raw_filter.as_ref()?.clone(),
            halved_framerate,
            video_info,
            session_id: session_id.to_string(),
            mitigation_mode: WebRTCSinkMitigationMode::NONE,
            transceiver,
        })
    }

    fn bitrate(&self) -> i32 {
        match self.factory_name.as_str() {
            "vp8enc" | "vp9enc" => self.element.property::<i32>("target-bitrate"),
            "x264enc" | "nvh264enc" | "vaapih264enc" | "vaapivp8enc" => {
                (self.element.property::<u32>("bitrate") * 1000) as i32
            }
            "nvv4l2h264enc" | "nvv4l2vp8enc" | "nvv4l2vp9enc" => {
                (self.element.property::<u32>("bitrate")) as i32
            }
            factory => unimplemented!("Factory {} is currently not supported", factory),
        }
    }

    fn scale_height_round_2(&self, height: i32) -> i32 {
        let ratio = gst_video::calculate_display_ratio(
            self.video_info.width(),
            self.video_info.height(),
            self.video_info.par(),
            gst::Fraction::new(1, 1),
        )
        .unwrap();

        let width = height.mul_div_ceil(ratio.numer(), ratio.denom()).unwrap();

        (width + 1) & !1
    }

    pub(crate) fn set_bitrate(&mut self, element: &super::BaseWebRTCSink, bitrate: i32) {
        match self.factory_name.as_str() {
            "vp8enc" | "vp9enc" => self.element.set_property("target-bitrate", bitrate),
            "x264enc" | "nvh264enc" | "vaapih264enc" | "vaapivp8enc" => self
                .element
                .set_property("bitrate", (bitrate / 1000) as u32),
            "nvv4l2h264enc" | "nvv4l2vp8enc" | "nvv4l2vp9enc" => {
                self.element.set_property("bitrate", bitrate as u32)
            }
            factory => unimplemented!("Factory {} is currently not supported", factory),
        }

        let current_caps = self.filter.property::<gst::Caps>("caps");
        let mut s = current_caps.structure(0).unwrap().to_owned();

        // Hardcoded thresholds, may be tuned further in the future, and
        // adapted according to the codec in use
        if bitrate < 500000 {
            let height = 360i32.min(self.video_info.height() as i32);
            let width = self.scale_height_round_2(height);

            s.set("height", height);
            s.set("width", width);

            if self.halved_framerate.numer() != 0 {
                s.set("framerate", self.halved_framerate);
            }

            self.mitigation_mode =
                WebRTCSinkMitigationMode::DOWNSAMPLED | WebRTCSinkMitigationMode::DOWNSCALED;
        } else if bitrate < 1000000 {
            let height = 360i32.min(self.video_info.height() as i32);
            let width = self.scale_height_round_2(height);

            s.set("height", height);
            s.set("width", width);
            s.remove_field("framerate");

            self.mitigation_mode = WebRTCSinkMitigationMode::DOWNSCALED;
        } else if bitrate < 2000000 {
            let height = 720i32.min(self.video_info.height() as i32);
            let width = self.scale_height_round_2(height);

            s.set("height", height);
            s.set("width", width);
            s.remove_field("framerate");

            self.mitigation_mode = WebRTCSinkMitigationMode::DOWNSCALED;
        } else {
            s.remove_field("height");
            s.remove_field("width");
            s.remove_field("framerate");

            self.mitigation_mode = WebRTCSinkMitigationMode::NONE;
        }

        let caps = gst::Caps::builder_full_with_any_features()
            .structure(s)
            .build();

        if !caps.is_strictly_equal(&current_caps) {
            gst::log!(
                CAT,
                obj: element,
                "session {}: setting bitrate {} and caps {} on encoder {:?}",
                self.session_id,
                bitrate,
                caps,
                self.element
            );

            self.filter.set_property("caps", caps);
        }
    }

    fn gather_stats(&self) -> gst::Structure {
        gst::Structure::builder("application/x-webrtcsink-video-encoder-stats")
            .field("bitrate", self.bitrate())
            .field("mitigation-mode", self.mitigation_mode)
            .field("codec-name", self.codec_name.as_str())
            .field(
                "fec-percentage",
                self.transceiver.property::<u32>("fec-percentage"),
            )
            .build()
    }
}

impl State {
    fn finalize_session(&mut self, session: &mut Session) {
        gst::info!(CAT, "Ending session {}", session.id);
        session.pipeline.debug_to_dot_file_with_ts(
            gst::DebugGraphDetails::all(),
            format!("removing-session-{}-", session.id),
        );

        for ssrc in session.webrtc_pads.keys() {
            session.links.remove(ssrc);
        }

        let stats_collection_handle = session.stats_collection_handle.take();

        let finalizing_sessions = self.finalizing_sessions.clone();
        let session_id = session.id.clone();
        let (sessions, _cvar) = &*finalizing_sessions;
        sessions.lock().unwrap().insert(session_id.clone());

        let pipeline = session.pipeline.clone();
        RUNTIME.spawn_blocking(move || {
            if let Some(stats_collection_handle) = stats_collection_handle {
                stats_collection_handle.abort();
                let _ = RUNTIME.block_on(stats_collection_handle);
            }

            let _ = pipeline.set_state(gst::State::Null);
            drop(pipeline);

            let (sessions, cvar) = &*finalizing_sessions;
            let mut sessions = sessions.lock().unwrap();
            sessions.remove(&session_id);
            cvar.notify_one();

            gst::debug!(CAT, "Session {session_id} ended");
        });
    }

    fn end_session(&mut self, session_id: &str) -> Option<Session> {
        if let Some(mut session) = self.sessions.remove(session_id) {
            self.finalize_session(&mut session);
            Some(session)
        } else {
            None
        }
    }

    fn should_start_signaller(&mut self, element: &super::BaseWebRTCSink) -> bool {
        self.signaller_state == SignallerState::Stopped
            && element.current_state() >= gst::State::Paused
            && self.codec_discovery_done
    }
}

impl Session {
    fn new(
        id: String,
        pipeline: gst::Pipeline,
        webrtcbin: gst::Element,
        peer_id: String,
        congestion_controller: Option<CongestionController>,
        rtpgccbwe: Option<gst::Element>,
        cc_info: CCInfo,
    ) -> Self {
        Self {
            id,
            pipeline,
            webrtcbin,
            peer_id,
            cc_info,
            rtprtxsend: None,
            congestion_controller,
            rtpgccbwe,
            stats: gst::Structure::new_empty("application/x-webrtc-stats"),
            sdp: None,
            webrtc_pads: HashMap::new(),
            encoders: Vec::new(),
            links: HashMap::new(),
            stats_sigid: None,
            codecs: None,
            stats_collection_handle: None,
        }
    }

    fn gather_stats(&self) -> gst::Structure {
        let mut ret = self.stats.to_owned();

        let encoder_stats = self
            .encoders
            .iter()
            .map(VideoEncoder::gather_stats)
            .map(|s| s.to_send_value())
            .collect::<gst::Array>();

        let our_stats = gst::Structure::builder("application/x-webrtcsink-consumer-stats")
            .field("video-encoders", encoder_stats)
            .build();

        ret.set("consumer-stats", our_stats);

        ret
    }

    /// Called when we have received an answer, connects an InputStream
    /// to a given WebRTCPad
    fn connect_input_stream(
        &mut self,
        element: &super::BaseWebRTCSink,
        producer: &StreamProducer,
        webrtc_pad: &WebRTCPad,
        codecs: &BTreeMap<i32, Codec>,
    ) -> Result<(), Error> {
        // No stream name, pad only exists to deactivate media
        let stream_name = match webrtc_pad.stream_name {
            Some(ref name) => name,
            None => {
                gst::info!(
                    CAT,
                    obj: element,
                    "Consumer {} not connecting any input stream for inactive media {}",
                    self.peer_id,
                    webrtc_pad.media_idx
                );
                return Ok(());
            }
        };

        gst::info!(
            CAT,
            obj: element,
            "Connecting input stream {} for consumer {} and media {}",
            stream_name,
            self.peer_id,
            webrtc_pad.media_idx
        );

        let payload = webrtc_pad.payload.unwrap();

        let codec = match self.codecs {
            Some(ref codecs) => {
                gst::debug!(CAT, obj: element, "Picking codec from remote offer");

                codecs
                    .get(&payload)
                    .cloned()
                    .ok_or_else(|| anyhow!("No codec for payload {}", payload))?
            }
            None => {
                gst::debug!(CAT, obj: element, "Picking codec from local offer");

                codecs
                    .get(&payload)
                    .cloned()
                    .ok_or_else(|| anyhow!("No codec for payload {}", payload))?
            }
        };

        let appsrc = make_element("appsrc", Some(stream_name))?;
        self.pipeline.add(&appsrc).unwrap();

        let pay_filter = make_element("capsfilter", None)?;
        self.pipeline.add(&pay_filter).unwrap();

        let output_caps = codec.output_filter().unwrap_or_else(gst::Caps::new_any);

        let encoding_chain = EncodingChainBuilder::new(
            &webrtc_pad.in_caps,
            &output_caps,
            &codec,
            element.emit_by_name::<Option<gst::Element>>(
                "request-encoded-filter",
                &[&Some(&self.peer_id), &stream_name, &codec.caps],
            ),
        )
        .ssrc(webrtc_pad.ssrc)
        .build(&self.pipeline, &appsrc)?;

        if let Some(ref enc) = encoding_chain.encoder {
            element.emit_by_name::<bool>("encoder-setup", &[&self.peer_id, &stream_name, &enc]);
        }

        // At this point, the peer has provided its answer, and we want to
        // let the payloader / encoder perform negotiation according to that.
        //
        // This means we need to unset our codec preferences, as they would now
        // conflict with what the peer actually requested (see webrtcbin's
        // caps query implementation), and instead install a capsfilter downstream
        // of the payloader with caps constructed from the relevant SDP media.
        let transceiver = webrtc_pad
            .pad
            .property::<gst_webrtc::WebRTCRTPTransceiver>("transceiver");
        transceiver.set_property("codec-preferences", None::<gst::Caps>);

        let mut global_caps = gst::Caps::new_empty_simple("application/x-unknown");

        let sdp = self.sdp.as_ref().unwrap();
        let sdp_media = sdp.media(webrtc_pad.media_idx).unwrap();

        sdp.attributes_to_caps(global_caps.get_mut().unwrap())
            .unwrap();
        sdp_media
            .attributes_to_caps(global_caps.get_mut().unwrap())
            .unwrap();

        let caps = sdp_media
            .caps_from_media(payload)
            .unwrap()
            .intersect(&global_caps);
        let s = caps.structure(0).unwrap();
        let mut filtered_s = gst::Structure::new_empty("application/x-rtp");

        filtered_s.extend(s.iter().filter_map(|(key, value)| {
            if key.starts_with("a-") {
                None
            } else {
                Some((key, value.to_owned()))
            }
        }));
        filtered_s.set("ssrc", webrtc_pad.ssrc);

        let caps = gst::Caps::builder_full().structure(filtered_s).build();

        pay_filter.set_property("caps", caps);

        if codec.is_video() {
            let video_info = gst_video::VideoInfo::from_caps(&webrtc_pad.in_caps)?;
            if let Some(mut enc) = VideoEncoder::new(
                &encoding_chain,
                video_info,
                &self.id,
                codec.caps.structure(0).unwrap().name(),
                transceiver,
            ) {
                match self.cc_info.heuristic {
                    WebRTCSinkCongestionControl::Disabled => {
                        // If congestion control is disabled, we simply use the highest
                        // known "safe" value for the bitrate.
                        enc.set_bitrate(element, self.cc_info.max_bitrate as i32);
                        enc.transceiver.set_property("fec-percentage", 50u32);
                    }
                    WebRTCSinkCongestionControl::Homegrown => {
                        if let Some(congestion_controller) = self.congestion_controller.as_mut() {
                            congestion_controller.target_bitrate_on_delay += enc.bitrate();
                            congestion_controller.target_bitrate_on_loss =
                                congestion_controller.target_bitrate_on_delay;
                            enc.transceiver.set_property("fec-percentage", 0u32);
                        } else {
                            /* If congestion control is disabled, we simply use the highest
                             * known "safe" value for the bitrate. */
                            enc.set_bitrate(element, self.cc_info.max_bitrate as i32);
                            enc.transceiver.set_property("fec-percentage", 50u32);
                        }
                    }
                    _ => enc.transceiver.set_property("fec-percentage", 0u32),
                }

                self.encoders.push(enc);

                if let Some(rtpgccbwe) = self.rtpgccbwe.as_ref() {
                    let max_bitrate = self.cc_info.max_bitrate * (self.encoders.len() as u32);
                    rtpgccbwe.set_property("max-bitrate", max_bitrate);
                }
            }
        }

        let appsrc = appsrc.downcast::<gst_app::AppSrc>().unwrap();
        gst_utils::StreamProducer::configure_consumer(&appsrc);
        self.pipeline
            .sync_children_states()
            .with_context(|| format!("Connecting input stream for {}", self.peer_id))?;

        encoding_chain.pay_filter.link(&pay_filter)?;

        let srcpad = pay_filter.static_pad("src").unwrap();

        srcpad
            .link(&webrtc_pad.pad)
            .with_context(|| format!("Connecting input stream for {}", self.peer_id))?;

        match producer.add_consumer(&appsrc) {
            Ok(link) => {
                self.links.insert(webrtc_pad.ssrc, link);
                Ok(())
            }
            Err(err) => Err(anyhow!("Could not link producer: {:?}", err)),
        }
    }
}

impl Drop for PipelineWrapper {
    fn drop(&mut self) {
        let _ = self.0.set_state(gst::State::Null);
    }
}

impl InputStream {
    /// Called when transitioning state up to Paused
    fn prepare(&mut self, element: &super::BaseWebRTCSink) -> Result<(), Error> {
        let clocksync = make_element("clocksync", None)?;
        let appsink = make_element("appsink", None)?
            .downcast::<gst_app::AppSink>()
            .unwrap();

        element.add(&clocksync).unwrap();
        element.add(&appsink).unwrap();

        clocksync
            .link(&appsink)
            .with_context(|| format!("Linking input stream {}", self.sink_pad.name()))?;

        element
            .sync_children_states()
            .with_context(|| format!("Linking input stream {}", self.sink_pad.name()))?;

        self.sink_pad
            .set_target(Some(&clocksync.static_pad("sink").unwrap()))
            .unwrap();

        self.producer = Some(StreamProducer::from(&appsink));

        Ok(())
    }

    /// Called when transitioning state back down to Ready
    fn unprepare(&mut self, element: &super::BaseWebRTCSink) {
        self.sink_pad.set_target(None::<&gst::Pad>).unwrap();

        if let Some(clocksync) = self.clocksync.take() {
            element.remove(&clocksync).unwrap();
            clocksync.set_state(gst::State::Null).unwrap();
        }

        if let Some(producer) = self.producer.take() {
            let appsink = producer.appsink().upcast_ref::<gst::Element>();
            element.remove(appsink).unwrap();
            appsink.set_state(gst::State::Null).unwrap();
        }
    }

    fn create_discovery(&mut self, type_: DiscoveryType) -> DiscoveryInfo {
        let discovery_info = DiscoveryInfo::new(
            type_,
            self.in_caps.clone().expect(
                "We should never create a discovery for a stream that doesn't have caps set",
            ),
        );

        self.discoveries.push(discovery_info.clone());

        discovery_info
    }

    fn remove_discovery(&mut self, discovery: &DiscoveryInfo) {
        let id = self
            .discoveries
            .iter()
            .position(|d| d.id == discovery.id)
            .expect("We expect discovery to always be in the list of discoverers when removing");
        self.discoveries.remove(id);
    }
}

impl NavigationEventHandler {
    fn new(element: &super::BaseWebRTCSink, webrtcbin: &gst::Element) -> Self {
        gst::info!(CAT, "Creating navigation data channel");
        let channel = webrtcbin.emit_by_name::<WebRTCDataChannel>(
            "create-data-channel",
            &[
                &"input",
                &gst::Structure::builder("config")
                    .field("priority", gst_webrtc::WebRTCPriorityType::High)
                    .build(),
            ],
        );

        let weak_element = element.downgrade();
        Self((
            channel.connect("on-message-string", false, move |values| {
                if let Some(element) = weak_element.upgrade() {
                    let _channel = values[0].get::<WebRTCDataChannel>().unwrap();
                    let msg = values[1].get::<&str>().unwrap();
                    create_navigation_event(&element, msg);
                }

                None
            }),
            channel,
        ))
    }
}

impl BaseWebRTCSink {
    fn generate_ssrc(
        element: &super::BaseWebRTCSink,
        webrtc_pads: &HashMap<u32, WebRTCPad>,
    ) -> u32 {
        loop {
            let ret = fastrand::u32(..);

            if !webrtc_pads.contains_key(&ret) {
                gst::trace!(CAT, obj: element, "Selected ssrc {}", ret);
                return ret;
            }
        }
    }

    fn request_inactive_webrtcbin_pad(
        element: &super::BaseWebRTCSink,
        webrtcbin: &gst::Element,
        webrtc_pads: &mut HashMap<u32, WebRTCPad>,
        is_video: bool,
    ) {
        let ssrc = BaseWebRTCSink::generate_ssrc(element, webrtc_pads);
        let media_idx = webrtc_pads.len() as i32;

        let pad = webrtcbin
            .request_pad_simple(&format!("sink_{}", media_idx))
            .unwrap();

        let transceiver = pad.property::<gst_webrtc::WebRTCRTPTransceiver>("transceiver");

        transceiver.set_property(
            "direction",
            gst_webrtc::WebRTCRTPTransceiverDirection::Inactive,
        );

        let payloader_caps = gst::Caps::builder("application/x-rtp")
            .field("media", if is_video { "video" } else { "audio" })
            .build();

        transceiver.set_property("codec-preferences", &payloader_caps);

        webrtc_pads.insert(
            ssrc,
            WebRTCPad {
                pad,
                in_caps: gst::Caps::new_empty(),
                media_idx: media_idx as u32,
                ssrc,
                stream_name: None,
                payload: None,
            },
        );
    }

    async fn request_webrtcbin_pad(
        element: &super::BaseWebRTCSink,
        webrtcbin: &gst::Element,
        stream: &mut InputStream,
        media: Option<&gst_sdp::SDPMediaRef>,
        settings: &Settings,
        webrtc_pads: &mut HashMap<u32, WebRTCPad>,
        codecs: &mut BTreeMap<i32, Codec>,
    ) {
        let ssrc = BaseWebRTCSink::generate_ssrc(element, webrtc_pads);
        let media_idx = webrtc_pads.len() as i32;

        let mut payloader_caps = match media {
            Some(media) => {
                let discovery_info = stream.create_discovery(DiscoveryType::CodecSelection);

                let codec = BaseWebRTCSink::select_codec(
                    element,
                    &discovery_info,
                    media,
                    &stream.in_caps.as_ref().unwrap().clone(),
                    &stream.sink_pad.name(),
                    settings,
                )
                .await;

                stream.remove_discovery(&discovery_info);

                match codec {
                    Some(codec) => {
                        gst::debug!(
                            CAT,
                            obj: element,
                            "Selected {codec:?} for media {media_idx}"
                        );

                        codecs.insert(codec.payload().unwrap(), codec.clone());
                        codec.output_filter().unwrap()
                    }
                    None => {
                        gst::error!(CAT, obj: element, "No codec selected for media {media_idx}");

                        gst::Caps::new_empty()
                    }
                }
            }
            None => stream.out_caps.as_ref().unwrap().to_owned(),
        };

        if payloader_caps.is_empty() {
            BaseWebRTCSink::request_inactive_webrtcbin_pad(
                element,
                webrtcbin,
                webrtc_pads,
                stream.is_video,
            );
        } else {
            let payloader_caps_mut = payloader_caps.make_mut();
            payloader_caps_mut.set("ssrc", ssrc);

            gst::info!(
                CAT,
                obj: element,
                "Requesting WebRTC pad with caps {}",
                payloader_caps
            );

            let pad = webrtcbin
                .request_pad_simple(&format!("sink_{}", media_idx))
                .unwrap();

            let transceiver = pad.property::<gst_webrtc::WebRTCRTPTransceiver>("transceiver");

            transceiver.set_property(
                "direction",
                gst_webrtc::WebRTCRTPTransceiverDirection::Sendonly,
            );

            transceiver.set_property("codec-preferences", &payloader_caps);

            if stream.sink_pad.name().starts_with("video_") {
                if settings.do_fec {
                    transceiver.set_property("fec-type", gst_webrtc::WebRTCFECType::UlpRed);
                }

                transceiver.set_property("do-nack", settings.do_retransmission);
            }

            webrtc_pads.insert(
                ssrc,
                WebRTCPad {
                    pad,
                    in_caps: stream.in_caps.as_ref().unwrap().clone(),
                    media_idx: media_idx as u32,
                    ssrc,
                    stream_name: Some(stream.sink_pad.name().to_string()),
                    payload: None,
                },
            );
        }
    }

    /// Prepare for accepting consumers, by setting
    /// up StreamProducers for each of our sink pads
    fn prepare(&self, element: &super::BaseWebRTCSink) -> Result<(), Error> {
        gst::debug!(CAT, obj: element, "preparing");

        self.state
            .lock()
            .unwrap()
            .streams
            .iter_mut()
            .try_for_each(|(_, stream)| stream.prepare(element))?;

        Ok(())
    }

    /// Unprepare by stopping consumers, then the signaller object.
    /// Might abort codec discovery
    fn unprepare(&self, element: &super::BaseWebRTCSink) -> Result<(), Error> {
        gst::info!(CAT, obj: element, "unpreparing");

        let settings = self.settings.lock().unwrap();
        let signaller = settings.signaller.clone();
        drop(settings);
        let mut state = self.state.lock().unwrap();

        let session_ids: Vec<_> = state.sessions.keys().map(|k| k.to_owned()).collect();

        let sessions: Vec<_> = session_ids
            .iter()
            .filter_map(|id| state.end_session(id))
            .collect();

        state
            .streams
            .iter_mut()
            .for_each(|(_, stream)| stream.unprepare(element));

        let codecs_abort_handle = std::mem::take(&mut state.codecs_abort_handles);
        codecs_abort_handle.into_iter().for_each(|handle| {
            handle.abort();
        });

        gst::debug!(CAT, obj: element, "Waiting for codec discoveries to finish");
        let codecs_done_receiver = std::mem::take(&mut state.codecs_done_receivers);
        codecs_done_receiver.into_iter().for_each(|receiver| {
            RUNTIME.block_on(async {
                let _ = receiver.await;
            });
        });
        gst::debug!(CAT, obj: element, "No codec discovery is running anymore");

        state.codec_discovery_done = false;
        state.codecs = BTreeMap::new();

        let signaller_state = state.signaller_state;
        if state.signaller_state == SignallerState::Started {
            state.signaller_state = SignallerState::Stopped;
        }

        drop(state);
        gst::debug!(CAT, obj: element, "Ending sessions");
        for session in sessions {
            signaller.end_session(&session.id);
        }
        gst::debug!(CAT, obj: element, "All sessions have started finalizing");

        if signaller_state == SignallerState::Started {
            gst::info!(CAT, obj: element, "Stopping signaller");
            signaller.stop();
            gst::info!(CAT, obj: element, "Stopped signaller");
        }

        let finalizing_sessions = self.state.lock().unwrap().finalizing_sessions.clone();

        let (sessions, cvar) = &*finalizing_sessions;
        let mut sessions = sessions.lock().unwrap();
        while !sessions.is_empty() {
            sessions = cvar.wait(sessions).unwrap();
        }

        gst::debug!(CAT, obj: element, "All sessions are done finalizing");

        Ok(())
    }

    fn connect_signaller(&self, signaler: &Signallable) {
        let instance = &*self.obj();

        let _ = self.state.lock().unwrap().signaller_signals.insert(SignallerSignals {
            error: signaler.connect_closure(
                "error",
                false,
                glib::closure!(@watch instance => move |_signaler: glib::Object, error: String| {
                    gst::element_error!(
                        instance,
                        gst::StreamError::Failed,
                        ["Signalling error: {}", error]
                    );
                })
            ),

            request_meta: signaler.connect_closure(
                "request-meta",
                false,
                glib::closure!(@watch instance => move |_signaler: glib::Object| -> Option<gst::Structure> {
                    let meta = instance.imp().settings.lock().unwrap().meta.clone();

                    meta
                })
            ),

            session_requested: signaler.connect_closure(
                "session-requested",
                false,
                glib::closure!(@watch instance => move |_signaler: glib::Object, session_id: &str, peer_id: &str, offer: Option<&gst_webrtc::WebRTCSessionDescription>|{
                    if let Err(err) = instance.imp().start_session(session_id, peer_id, offer) {
                        gst::warning!(CAT, "{}", err);
                    }
                })
            ),

            session_description: signaler.connect_closure(
                "session-description",
                false,
                glib::closure!(@watch instance => move |
                        _signaler: glib::Object,
                        session_id: &str,
                        session_description: &gst_webrtc::WebRTCSessionDescription| {

                        if session_description.type_() == gst_webrtc::WebRTCSDPType::Answer {
                            instance.imp().handle_sdp_answer(instance, session_id, session_description);
                        } else {
                            gst::error!(CAT, obj: instance, "Unsupported SDP Type");
                        }
                    }
                ),
            ),

            handle_ice: signaler.connect_closure(
                    "handle-ice",
                    false,
                    glib::closure!(@watch instance => move |
                        _signaler: glib::Object,
                        session_id: &str,
                        sdp_m_line_index: u32,
                        _sdp_mid: Option<String>,
                        candidate: &str| {
                        instance
                            .imp()
                            .handle_ice(session_id, Some(sdp_m_line_index), None, candidate);
                    }),
                ),

            session_ended: signaler.connect_closure(
                "session-ended",
                false,
                glib::closure!(@watch instance => move |_signaler: glib::Object, session_id: &str|{
                    if let Err(err) = instance.imp().remove_session(instance, session_id, false) {
                        gst::warning!(CAT, "{}", err);
                    }
                    false
                })
            ),

            shutdown: signaler.connect_closure(
                "shutdown",
                false,
                glib::closure!(@watch instance => move |_signaler: glib::Object|{
                    instance.imp().shutdown(instance);
                })
            ),
        });
    }

    /// When using a custom signaller
    pub fn set_signaller(&self, signaller: Signallable) -> Result<(), Error> {
        let sigobj = signaller.clone();
        let mut settings = self.settings.lock().unwrap();

        self.connect_signaller(&sigobj);
        settings.signaller = signaller;

        Ok(())
    }

    /// Called by the signaller when it wants to shut down gracefully
    fn shutdown(&self, element: &super::BaseWebRTCSink) {
        gst::info!(CAT, "Shutting down");
        let _ = element.post_message(gst::message::Eos::builder().src(element).build());
    }

    fn on_offer_created(
        &self,
        _element: &super::BaseWebRTCSink,
        offer: gst_webrtc::WebRTCSessionDescription,
        session_id: &str,
    ) {
        let settings = self.settings.lock().unwrap();
        let signaller = settings.signaller.clone();
        drop(settings);
        let state = self.state.lock().unwrap();

        if let Some(session) = state.sessions.get(session_id) {
            session
                .webrtcbin
                .emit_by_name::<()>("set-local-description", &[&offer, &None::<gst::Promise>]);
            drop(state);

            signaller.send_sdp(session_id, &offer);
        }
    }

    fn on_answer_created(
        &self,
        element: &super::BaseWebRTCSink,
        answer: gst_webrtc::WebRTCSessionDescription,
        session_id: &str,
    ) {
        let settings = self.settings.lock().unwrap();
        let signaller = settings.signaller.clone();
        drop(settings);
        let mut state = self.state.lock().unwrap();

        if let Some(mut session) = state.sessions.remove(session_id) {
            let sdp = answer.sdp();

            session.sdp = Some(sdp.to_owned());

            for webrtc_pad in session.webrtc_pads.values_mut() {
                webrtc_pad.payload = sdp
                    .media(webrtc_pad.media_idx)
                    .and_then(|media| media.format(0))
                    .and_then(|format| format.parse::<i32>().ok());
            }

            session
                .webrtcbin
                .emit_by_name::<()>("set-local-description", &[&answer, &None::<gst::Promise>]);

            let session_id = session.id.clone();

            state.sessions.insert(session.id.clone(), session);

            drop(state);
            signaller.send_sdp(&session_id, &answer);

            self.on_remote_description_set(element, session_id)
        }
    }

    fn on_remote_description_offer_set(&self, element: &super::BaseWebRTCSink, session_id: String) {
        let state = self.state.lock().unwrap();

        if let Some(session) = state.sessions.get(&session_id) {
            let element = element.downgrade();
            gst::debug!(CAT, "Creating answer for session {}", session_id);
            let session_id = session_id.clone();
            let promise = gst::Promise::with_change_func(move |reply| {
                gst::debug!(CAT, "Created answer for session {}", session_id);

                if let Some(element) = element.upgrade() {
                    let this = element.imp();
                    let reply = match reply {
                        Ok(Some(reply)) => reply,
                        Ok(None) => {
                            gst::warning!(
                                CAT,
                                obj: element,
                                "Promise returned without a reply for {}",
                                session_id
                            );
                            let _ = this.remove_session(&element, &session_id, true);
                            return;
                        }
                        Err(err) => {
                            gst::warning!(
                                CAT,
                                obj: element,
                                "Promise returned with an error for {}: {:?}",
                                session_id,
                                err
                            );
                            let _ = this.remove_session(&element, &session_id, true);
                            return;
                        }
                    };

                    if let Ok(answer) = reply.value("answer").map(|answer| {
                        answer
                            .get::<gst_webrtc::WebRTCSessionDescription>()
                            .unwrap()
                    }) {
                        this.on_answer_created(&element, answer, &session_id);
                    } else {
                        gst::warning!(
                            CAT,
                            "Reply without an answer for session {}: {:?}",
                            session_id,
                            reply
                        );
                        let _ = this.remove_session(&element, &session_id, true);
                    }
                }
            });

            session
                .webrtcbin
                .emit_by_name::<()>("create-answer", &[&None::<gst::Structure>, &promise]);
        }
    }

    async fn select_codec(
        element: &super::BaseWebRTCSink,
        discovery_info: &DiscoveryInfo,
        media: &gst_sdp::SDPMediaRef,
        in_caps: &gst::Caps,
        stream_name: &str,
        settings: &Settings,
    ) -> Option<Codec> {
        let user_caps = match media.media() {
            Some("audio") => &settings.audio_caps,
            Some("video") => &settings.video_caps,
            _ => {
                unreachable!();
            }
        };

        // Here, we want to try the codecs proposed by the remote offerer
        // in the order requested by the user. For instance, if the offer
        // contained VP8, VP9 and H264 (in this order), but the video-caps
        // contained H264 and VP8 (in this order), we want to try H264 first,
        // skip VP9, then try VP8.
        //
        // If the user wants to simply use the offered order, they should be
        // able to set video-caps to ANY caps, though other tweaks are probably
        // required elsewhere to make this work in all cases (eg when we create
        // the offer).

        let mut ordered_codecs_and_caps: Vec<(gst::Caps, Vec<(Codec, gst::Caps)>)> = user_caps
            .iter()
            .map(|s| ([s.to_owned()].into_iter().collect(), Vec::new()))
            .collect();

        for (payload, mut caps) in media
            .formats()
            .filter_map(|format| format.parse::<i32>().ok())
            .filter_map(|payload| Some(payload).zip(media.caps_from_media(payload)))
        {
            let s = caps.make_mut().structure_mut(0).unwrap();

            s.filter_map_in_place(|quark, value| {
                if quark.as_str().starts_with("rtcp-fb-") {
                    None
                } else {
                    Some(value)
                }
            });
            s.set_name("application/x-rtp");

            let encoding_name = s.get::<String>("encoding-name").unwrap();

            if let Some(mut codec) = Codecs::find(&encoding_name) {
                if !codec.can_encode() {
                    continue;
                }

                codec.set_pt(payload);
                for (user_caps, codecs_and_caps) in ordered_codecs_and_caps.iter_mut() {
                    if codec.caps.is_subset(user_caps) {
                        codecs_and_caps.push((codec, caps));
                        break;
                    }
                }
            }
        }

        let mut twcc_idx = None;

        for attribute in media.attributes() {
            if attribute.key() == "extmap" {
                if let Some(value) = attribute.value() {
                    if let Some((idx_str, ext)) = value.split_once(' ') {
                        if ext == RTP_TWCC_URI {
                            if let Ok(idx) = idx_str.parse::<u32>() {
                                twcc_idx = Some(idx);
                            } else {
                                gst::warning!(
                                    CAT,
                                    obj: element,
                                    "Failed to parse twcc index: {idx_str}"
                                );
                            }
                        }
                    }
                }
            }
        }

        let futs = ordered_codecs_and_caps
            .iter()
            .flat_map(|(_, codecs_and_caps)| codecs_and_caps)
            .map(|(codec, caps)| async move {
                BaseWebRTCSink::run_discovery_pipeline(
                    element,
                    stream_name,
                    discovery_info,
                    codec.clone(),
                    in_caps.clone(),
                    caps,
                    twcc_idx,
                )
                .await
                .map(|s| {
                    let mut codec = codec.clone();
                    codec.set_output_filter([s].into_iter().collect());
                    codec
                })
            });

        /* Run sequentially to avoid NVENC collisions */
        for fut in futs {
            if let Ok(codec) = fut.await {
                return Some(codec);
            }
        }

        None
    }

    fn negotiate(
        &self,
        element: &super::BaseWebRTCSink,
        session_id: &str,
        offer: Option<&gst_webrtc::WebRTCSessionDescription>,
    ) {
        let state = self.state.lock().unwrap();

        gst::debug!(CAT, obj: element, "Negotiating for session {}", session_id);

        if let Some(session) = state.sessions.get(session_id) {
            gst::trace!(CAT, "WebRTC pads: {:?}", session.webrtc_pads);

            if let Some(offer) = offer {
                let element = element.downgrade();
                let session_id = session_id.to_string();

                let promise = gst::Promise::with_change_func(move |reply| {
                    gst::debug!(CAT, "received reply {:?}", reply);
                    if let Some(element) = element.upgrade() {
                        let this = element.imp();

                        this.on_remote_description_offer_set(&element, session_id);
                    }
                });

                session
                    .webrtcbin
                    .emit_by_name::<()>("set-remote-description", &[&offer, &promise]);
            } else {
                let element = element.downgrade();
                gst::debug!(CAT, "Creating offer for session {}", session_id);
                let session_id = session_id.to_string();
                let promise = gst::Promise::with_change_func(move |reply| {
                    gst::debug!(CAT, "Created offer for session {}", session_id);

                    if let Some(element) = element.upgrade() {
                        let this = element.imp();
                        let reply = match reply {
                            Ok(Some(reply)) => reply,
                            Ok(None) => {
                                gst::warning!(
                                    CAT,
                                    obj: element,
                                    "Promise returned without a reply for {}",
                                    session_id
                                );
                                let _ = this.remove_session(&element, &session_id, true);
                                return;
                            }
                            Err(err) => {
                                gst::warning!(
                                    CAT,
                                    obj: element,
                                    "Promise returned with an error for {}: {:?}",
                                    session_id,
                                    err
                                );
                                let _ = this.remove_session(&element, &session_id, true);
                                return;
                            }
                        };

                        if let Ok(offer) = reply.value("offer").map(|offer| {
                            offer.get::<gst_webrtc::WebRTCSessionDescription>().unwrap()
                        }) {
                            this.on_offer_created(&element, offer, &session_id);
                        } else {
                            gst::warning!(
                                CAT,
                                "Reply without an offer for session {}: {:?}",
                                session_id,
                                reply
                            );
                            let _ = this.remove_session(&element, &session_id, true);
                        }
                    }
                });

                session
                    .webrtcbin
                    .emit_by_name::<()>("create-offer", &[&None::<gst::Structure>, &promise]);
            }
        } else {
            gst::debug!(
                CAT,
                obj: element,
                "consumer for session {} no longer exists (sessions: {:?}",
                session_id,
                state.sessions.keys()
            );
        }
    }

    fn on_ice_candidate(
        &self,
        _element: &super::BaseWebRTCSink,
        session_id: String,
        sdp_m_line_index: u32,
        candidate: String,
    ) {
        let settings = self.settings.lock().unwrap();
        let signaller = settings.signaller.clone();
        drop(settings);
        signaller.add_ice(&session_id, &candidate, sdp_m_line_index, None)
    }

    /// Called by the signaller to add a new session
    fn start_session(
        &self,
        session_id: &str,
        peer_id: &str,
        offer: Option<&gst_webrtc::WebRTCSessionDescription>,
    ) -> Result<(), WebRTCSinkError> {
        let pipeline = gst::Pipeline::builder()
            .name(format!("session-pipeline-{session_id}"))
            .build();

        self.obj()
            .emit_by_name::<()>("consumer-pipeline-created", &[&peer_id, &pipeline]);

        let settings = self.settings.lock().unwrap();
        let mut state = self.state.lock().unwrap();
        let peer_id = peer_id.to_string();
        let session_id = session_id.to_string();
        let element = self.obj().clone();

        if state.sessions.contains_key(&session_id) {
            return Err(WebRTCSinkError::DuplicateSessionId(session_id));
        }

        gst::info!(
            CAT,
            obj: element,
            "Adding session: {} for peer: {}",
            session_id,
            peer_id,
        );

        let webrtcbin = make_element("webrtcbin", Some(&format!("webrtcbin-{session_id}")))
            .map_err(|err| WebRTCSinkError::SessionPipelineError {
                session_id: session_id.clone(),
                peer_id: peer_id.clone(),
                details: err.to_string(),
            })?;

        webrtcbin.set_property_from_str("bundle-policy", "max-bundle");
        webrtcbin.set_property("ice-transport-policy", settings.ice_transport_policy);

        if let Some(stun_server) = settings.stun_server.as_ref() {
            webrtcbin.set_property("stun-server", stun_server);
        }

        for turn_server in settings.turn_servers.iter() {
            webrtcbin.emit_by_name::<bool>("add-turn-server", &[&turn_server]);
        }

        let rtpgccbwe = match settings.cc_info.heuristic {
            WebRTCSinkCongestionControl::GoogleCongestionControl => {
                let rtpgccbwe = match gst::ElementFactory::make("rtpgccbwe").build() {
                    Err(err) => {
                        glib::g_warning!(
                            "webrtcsink",
                            "The `rtpgccbwe` element is not available \
                            not doing any congestion control: {err:?}"
                        );
                        None
                    }
                    Ok(cc) => {
                        webrtcbin.connect_closure(
                            "request-aux-sender",
                            false,
                            glib::closure!(@watch element, @strong session_id, @weak-allow-none cc
                                    => move |_webrtcbin: gst::Element, _transport: gst::Object| {
                                if let Some(ref cc) = cc {
                                    let settings = element.imp().settings.lock().unwrap();

                                    // TODO: Bind properties with @element's
                                    cc.set_properties(&[
                                        ("min-bitrate", &settings.cc_info.min_bitrate),
                                        ("estimated-bitrate", &settings.cc_info.start_bitrate),
                                        ("max-bitrate", &settings.cc_info.max_bitrate),
                                    ]);

                                    cc.connect_notify(Some("estimated-bitrate"),
                                        glib::clone!(@weak element, @strong session_id
                                            => move |bwe, pspec| {
                                            element.imp().set_bitrate(&element, &session_id,
                                                bwe.property::<u32>(pspec.name()));
                                        }
                                    ));
                                }

                                cc
                            }),
                        );

                        Some(cc)
                    }
                };

                webrtcbin.connect_closure(
                    "deep-element-added",
                    false,
                    glib::closure!(@watch element, @strong session_id
                            => move |_webrtcbin: gst::Element, _bin: gst::Bin, e: gst::Element| {

                        if e.factory().map_or(false, |f| f.name() == "rtprtxsend") {
                            if e.has_property("stuffing-kbps", Some(i32::static_type())) {
                                element.imp().set_rtptrxsend(element, &session_id, e);
                            } else {
                                gst::warning!(CAT, "rtprtxsend doesn't have a `stuffing-kbps` \
                                    property, stuffing disabled");
                            }
                        }
                    }),
                );

                rtpgccbwe
            }
            _ => None,
        };

        pipeline.add(&webrtcbin).unwrap();

        let element_clone = element.downgrade();
        let session_id_clone = session_id.clone();
        webrtcbin.connect("on-ice-candidate", false, move |values| {
            if let Some(element) = element_clone.upgrade() {
                let this = element.imp();
                let sdp_m_line_index = values[1].get::<u32>().expect("Invalid argument");
                let candidate = values[2].get::<String>().expect("Invalid argument");
                this.on_ice_candidate(
                    &element,
                    session_id_clone.to_string(),
                    sdp_m_line_index,
                    candidate,
                );
            }
            None
        });

        let element_clone = element.downgrade();
        let peer_id_clone = peer_id.clone();
        let session_id_clone = session_id.clone();
        webrtcbin.connect_notify(Some("connection-state"), move |webrtcbin, _pspec| {
            if let Some(element) = element_clone.upgrade() {
                let state =
                    webrtcbin.property::<gst_webrtc::WebRTCPeerConnectionState>("connection-state");

                match state {
                    gst_webrtc::WebRTCPeerConnectionState::Failed => {
                        let this = element.imp();
                        gst::warning!(
                            CAT,
                            obj: element,
                            "Connection state for in session {} (peer {}) failed",
                            session_id_clone,
                            peer_id_clone
                        );
                        let _ = this.remove_session(&element, &session_id_clone, true);
                    }
                    _ => {
                        gst::log!(
                            CAT,
                            obj: element,
                            "Connection state in session {} (peer {}) changed: {:?}",
                            session_id_clone,
                            peer_id_clone,
                            state
                        );
                    }
                }
            }
        });

        let element_clone = element.downgrade();
        let peer_id_clone = peer_id.clone();
        let session_id_clone = session_id.clone();
        webrtcbin.connect_notify(Some("ice-connection-state"), move |webrtcbin, _pspec| {
            if let Some(element) = element_clone.upgrade() {
                let state = webrtcbin
                    .property::<gst_webrtc::WebRTCICEConnectionState>("ice-connection-state");
                let this = element.imp();

                match state {
                    gst_webrtc::WebRTCICEConnectionState::Failed => {
                        gst::warning!(
                            CAT,
                            obj: element,
                            "Ice connection state in session {} (peer {}) failed",
                            session_id_clone,
                            peer_id_clone,
                        );
                        let _ = this.remove_session(&element, &session_id_clone, true);
                    }
                    _ => {
                        gst::log!(
                            CAT,
                            obj: element,
                            "Ice connection state in session {} (peer {}) changed: {:?}",
                            session_id_clone,
                            peer_id_clone,
                            state
                        );
                    }
                }

                if state == gst_webrtc::WebRTCICEConnectionState::Completed {
                    let state = this.state.lock().unwrap();

                    if let Some(session) = state.sessions.get(&session_id_clone) {
                        for webrtc_pad in session.webrtc_pads.values() {
                            if let Some(srcpad) = webrtc_pad.pad.peer() {
                                srcpad.send_event(
                                    gst_video::UpstreamForceKeyUnitEvent::builder()
                                        .all_headers(true)
                                        .build(),
                                );
                            }
                        }
                    }
                }
            }
        });

        let element_clone = element.downgrade();
        let peer_id_clone = peer_id.clone();
        let session_id_clone = session_id.clone();
        webrtcbin.connect_notify(Some("ice-gathering-state"), move |webrtcbin, _pspec| {
            let state =
                webrtcbin.property::<gst_webrtc::WebRTCICEGatheringState>("ice-gathering-state");

            if let Some(element) = element_clone.upgrade() {
                gst::log!(
                    CAT,
                    obj: element,
                    "Ice gathering state in session {} (peer {}) changed: {:?}",
                    session_id_clone,
                    peer_id_clone,
                    state
                );
            }
        });

        let session = Session::new(
            session_id.clone(),
            pipeline.clone(),
            webrtcbin.clone(),
            peer_id.clone(),
            match settings.cc_info.heuristic {
                WebRTCSinkCongestionControl::Homegrown => Some(CongestionController::new(
                    &peer_id,
                    settings.cc_info.min_bitrate,
                    settings.cc_info.max_bitrate,
                )),
                _ => None,
            },
            rtpgccbwe,
            settings.cc_info,
        );

        let rtpbin = webrtcbin
            .dynamic_cast_ref::<gst::ChildProxy>()
            .unwrap()
            .child_by_name("rtpbin")
            .unwrap();

        if session.congestion_controller.is_some() {
            let session_id_str = session_id.to_string();
            rtpbin.connect_closure("on-new-ssrc", true,
                glib::closure!(@weak-allow-none element,
                                => move |rtpbin: gst::Object, session_id: u32, _src: u32| {
                        let rtp_session = rtpbin.emit_by_name::<gst::Element>("get-session", &[&session_id]);

                        let element = element.expect("on-new-ssrc emitted when webrtcsink has been disposed?");
                        let mut state = element.imp().state.lock().unwrap();
                        if let Some(session) = state.sessions.get_mut(&session_id_str) {

                            if session.stats_sigid.is_none() {
                                let session_id_str = session_id_str.clone();
                                let element = element.downgrade();
                                session.stats_sigid = Some(rtp_session.connect_notify(Some("twcc-stats"),
                                    move |sess, pspec| {
                                        if let Some(element) = element.upgrade() {
                                            // Run the Loss-based control algorithm on new peer TWCC feedbacks
                                            element.imp().process_loss_stats(&element, &session_id_str, &sess.property::<gst::Structure>(pspec.name()));
                                        }
                                    }
                                ));
                            }
                        }
                    })
                );
        }

        let clock = element.clock();

        pipeline.use_clock(clock.as_ref());
        pipeline.set_start_time(gst::ClockTime::NONE);
        pipeline.set_base_time(element.base_time().unwrap());

        let mut bus_stream = pipeline.bus().unwrap().stream();
        let element_clone = element.downgrade();
        let pipeline_clone = pipeline.downgrade();
        let session_id_clone = session_id.clone();

        RUNTIME.spawn(async move {
            while let Some(msg) = bus_stream.next().await {
                let Some(element) = element_clone.upgrade() else { break; };
                let Some(pipeline) = pipeline_clone.upgrade() else { break; };
                let this = element.imp();
                match msg.view() {
                    gst::MessageView::Error(err) => {
                        gst::error!(
                            CAT,
                            "session {} error: {}, details: {:?}",
                            session_id_clone,
                            err.error(),
                            err.debug()
                        );
                        let _ = this.remove_session(&element, &session_id_clone, true);
                    }
                    gst::MessageView::StateChanged(state_changed) => {
                        if state_changed.src() == Some(pipeline.upcast_ref()) {
                            pipeline.debug_to_dot_file_with_ts(
                                gst::DebugGraphDetails::all(),
                                format!(
                                    "webrtcsink-session-{}-{:?}-to-{:?}",
                                    session_id_clone,
                                    state_changed.old(),
                                    state_changed.current()
                                ),
                            );
                        }
                    }
                    gst::MessageView::Latency(..) => {
                        gst::info!(CAT, obj: pipeline, "Recalculating latency");
                        let _ = pipeline.recalculate_latency();
                    }
                    gst::MessageView::Eos(..) => {
                        gst::error!(
                            CAT,
                            "Unexpected end of stream in session {}",
                            session_id_clone,
                        );
                        let _ = this.remove_session(&element, &session_id_clone, true);
                    }
                    _ => (),
                }
            }
        });

        state.sessions.insert(session_id.to_string(), session);

        let mut streams: Vec<InputStream> = state.streams.values().cloned().collect();

        streams.sort_by_key(|s| s.serial);

        let element_clone = element.downgrade();
        let offer_clone = offer.cloned();
        RUNTIME.spawn(async move {
            if let Some(element) = element_clone.upgrade() {
                let this = element.imp();

                let settings_clone = this.settings.lock().unwrap().clone();
                let signaller = settings_clone.signaller.clone();

                let mut webrtc_pads: HashMap<u32, WebRTCPad> = HashMap::new();
                let mut codecs: BTreeMap<i32, Codec> = BTreeMap::new();

                if let Some(ref offer) = offer_clone {
                    for media in offer.sdp().medias() {
                        let media_is_video = match media.media() {
                            Some("audio") => false,
                            Some("video") => true,
                            _ => {
                                continue;
                            }
                        };

                        if let Some(idx) = streams.iter().position(|s| {
                            let structname =
                                s.in_caps.as_ref().unwrap().structure(0).unwrap().name();
                            let stream_is_video = structname.starts_with("video/");

                            if !stream_is_video {
                                assert!(structname.starts_with("audio/"));
                            }

                            media_is_video == stream_is_video
                        }) {
                            let mut stream = streams.remove(idx);
                            BaseWebRTCSink::request_webrtcbin_pad(
                                &element,
                                &webrtcbin,
                                &mut stream,
                                Some(media),
                                &settings_clone,
                                &mut webrtc_pads,
                                &mut codecs,
                            )
                            .await;
                        } else {
                            BaseWebRTCSink::request_inactive_webrtcbin_pad(
                                &element,
                                &webrtcbin,
                                &mut webrtc_pads,
                                media_is_video,
                            );
                        }
                    }
                } else {
                    for mut stream in streams {
                        BaseWebRTCSink::request_webrtcbin_pad(
                            &element,
                            &webrtcbin,
                            &mut stream,
                            None,
                            &settings_clone,
                            &mut webrtc_pads,
                            &mut codecs,
                        )
                        .await;
                    }
                }

                let enable_data_channel_navigation = settings_clone.enable_data_channel_navigation;

                drop(settings_clone);

                {
                    let mut state = this.state.lock().unwrap();
                    if let Some(mut session) = state.sessions.remove(&session_id) {
                        session.webrtc_pads = webrtc_pads;
                        if offer_clone.is_some() {
                            session.codecs = Some(codecs);
                        }
                        state.sessions.insert(session_id.to_owned(), session);
                    }
                }

                if let Err(err) = pipeline.set_state(gst::State::Ready) {
                    gst::warning!(
                        CAT,
                        obj: element,
                        "Failed to bring {peer_id} pipeline to READY: {}",
                        err
                    );
                    let _ = this.remove_session(&element, &session_id, true);
                    return;
                }

                if enable_data_channel_navigation {
                    let mut state = this.state.lock().unwrap();
                    state.navigation_handler =
                        Some(NavigationEventHandler::new(&element, &webrtcbin));
                }

                // This is intentionally emitted with the pipeline in the Ready state,
                // so that application code can create data channels at the correct
                // moment.
                element.emit_by_name::<()>("consumer-added", &[&peer_id, &webrtcbin]);
                signaller.emit_by_name::<()>("consumer-added", &[&peer_id, &webrtcbin]);

                // We don't connect to on-negotiation-needed, this in order to call the above
                // signal without holding the state lock:
                //
                // Going to Ready triggers synchronous emission of the on-negotiation-needed
                // signal, during which time the application may add a data channel, causing
                // renegotiation, which we do not support at this time.
                //
                // This is completely safe, as we know that by now all conditions are gathered:
                // webrtcbin is in the Ready state, and all its transceivers have codec_preferences.
                this.negotiate(&element, &session_id, offer_clone.as_ref());

                if let Err(err) = pipeline.set_state(gst::State::Playing) {
                    gst::warning!(
                        CAT,
                        obj: element,
                        "Failed to bring {peer_id} pipeline to PLAYING: {}",
                        err
                    );
                    let _ = this.remove_session(&element, &session_id, true);
                }
            }
        });

        Ok(())
    }

    /// Called by the signaller to remove a consumer
    fn remove_session(
        &self,
        element: &super::BaseWebRTCSink,
        session_id: &str,
        signal: bool,
    ) -> Result<(), WebRTCSinkError> {
        let settings = self.settings.lock().unwrap();
        let signaller = settings.signaller.clone();
        drop(settings);
        let mut state = self.state.lock().unwrap();

        if !state.sessions.contains_key(session_id) {
            return Err(WebRTCSinkError::NoSessionWithId(session_id.to_string()));
        }

        if let Some(session) = state.end_session(session_id) {
            drop(state);
            signaller
                .emit_by_name::<()>("consumer-removed", &[&session.peer_id, &session.webrtcbin]);
            if signal {
                signaller.end_session(session_id);
            }
            element.emit_by_name::<()>("consumer-removed", &[&session.peer_id, &session.webrtcbin]);
        }

        Ok(())
    }

    fn process_loss_stats(
        &self,
        element: &super::BaseWebRTCSink,
        session_id: &str,
        stats: &gst::Structure,
    ) {
        let mut state = element.imp().state.lock().unwrap();
        if let Some(session) = state.sessions.get_mut(session_id) {
            if let Some(congestion_controller) = session.congestion_controller.as_mut() {
                congestion_controller.loss_control(element, stats, &mut session.encoders);
            }
            session.stats = stats.to_owned();
        }
    }

    fn process_stats(
        &self,
        element: &super::BaseWebRTCSink,
        webrtcbin: gst::Element,
        session_id: &str,
    ) {
        let session_id = session_id.to_string();
        let promise = gst::Promise::with_change_func(
            glib::clone!(@strong session_id, @weak element => move |reply| {
                if let Ok(Some(stats)) = reply {

                    let mut state = element.imp().state.lock().unwrap();
                    if let Some(session) = state.sessions.get_mut(&session_id) {
                        if let Some(congestion_controller) = session.congestion_controller.as_mut() {
                            congestion_controller.delay_control(&element, stats, &mut session.encoders,);
                        }
                        session.stats = stats.to_owned();
                    }
                }
            }),
        );

        webrtcbin.emit_by_name::<()>("get-stats", &[&None::<gst::Pad>, &promise]);
    }

    fn set_rtptrxsend(
        &self,
        element: &super::BaseWebRTCSink,
        session_id: &str,
        rtprtxsend: gst::Element,
    ) {
        let mut state = element.imp().state.lock().unwrap();

        if let Some(session) = state.sessions.get_mut(session_id) {
            session.rtprtxsend = Some(rtprtxsend);
        }
    }

    fn set_bitrate(&self, element: &super::BaseWebRTCSink, session_id: &str, bitrate: u32) {
        let settings = element.imp().settings.lock().unwrap();
        let mut state = element.imp().state.lock().unwrap();

        if let Some(session) = state.sessions.get_mut(session_id) {
            let n_encoders = session.encoders.len();

            let fec_ratio = {
                if settings.do_fec && bitrate > DO_FEC_THRESHOLD {
                    (bitrate as f64 - DO_FEC_THRESHOLD as f64)
                        / ((session.cc_info.max_bitrate as usize * n_encoders) as f64
                            - DO_FEC_THRESHOLD as f64)
                } else {
                    0f64
                }
            };

            let fec_percentage = fec_ratio * 50f64;
            let encoders_bitrate =
                ((bitrate as f64) / (1. + (fec_percentage / 100.)) / (n_encoders as f64)) as i32;

            if let Some(rtpxsend) = session.rtprtxsend.as_ref() {
                rtpxsend.set_property("stuffing-kbps", (bitrate as f64 / 1000.) as i32);
            }

            for encoder in session.encoders.iter_mut() {
                encoder.set_bitrate(element, encoders_bitrate);
                encoder
                    .transceiver
                    .set_property("fec-percentage", (fec_percentage as u32).min(100));
            }
        }
    }

    fn on_remote_description_set(&self, element: &super::BaseWebRTCSink, session_id: String) {
        let mut state = self.state.lock().unwrap();
        let mut remove = false;
        let codecs = state.codecs.clone();

        if let Some(mut session) = state.sessions.remove(&session_id) {
            for webrtc_pad in session.webrtc_pads.clone().values() {
                let transceiver = webrtc_pad
                    .pad
                    .property::<gst_webrtc::WebRTCRTPTransceiver>("transceiver");

                let Some(ref stream_name) = webrtc_pad.stream_name else { continue; };

                if let Some(mid) = transceiver.mid() {
                    state.mids.insert(mid.to_string(), stream_name.clone());
                }

                if let Some(producer) = state
                    .streams
                    .get(stream_name)
                    .and_then(|stream| stream.producer.clone())
                {
                    drop(state);
                    if let Err(err) =
                        session.connect_input_stream(element, &producer, webrtc_pad, &codecs)
                    {
                        gst::error!(
                            CAT,
                            obj: element,
                            "Failed to connect input stream {} for session {}: {}",
                            stream_name,
                            session_id,
                            err
                        );
                        remove = true;
                        state = self.state.lock().unwrap();
                        break;
                    }
                    state = self.state.lock().unwrap();
                } else {
                    gst::error!(
                        CAT,
                        obj: element,
                        "No producer to connect session {} to",
                        session_id,
                    );
                    remove = true;
                    break;
                }
            }

            session.pipeline.debug_to_dot_file_with_ts(
                gst::DebugGraphDetails::all(),
                format!("webrtcsink-peer-{session_id}-remote-description-set",),
            );

            let element_clone = element.downgrade();
            let webrtcbin = session.webrtcbin.downgrade();
            let session_id_clone = session_id.clone();
            session.stats_collection_handle = Some(RUNTIME.spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_millis(100));

                loop {
                    interval.tick().await;
                    let element_clone = element_clone.clone();
                    if let (Some(webrtcbin), Some(element)) =
                        (webrtcbin.upgrade(), element_clone.upgrade())
                    {
                        element
                            .imp()
                            .process_stats(&element, webrtcbin, &session_id_clone);
                    } else {
                        break;
                    }
                }
            }));

            if remove {
                state.finalize_session(&mut session);
                drop(state);
                let settings = self.settings.lock().unwrap();
                let signaller = settings.signaller.clone();
                drop(settings);
                signaller.end_session(&session_id);
            } else {
                state.sessions.insert(session.id.clone(), session);
            }
        }
    }

    /// Called by the signaller with an ice candidate
    fn handle_ice(
        &self,
        session_id: &str,
        sdp_m_line_index: Option<u32>,
        _sdp_mid: Option<String>,
        candidate: &str,
    ) {
        let state = self.state.lock().unwrap();

        let sdp_m_line_index = match sdp_m_line_index {
            Some(sdp_m_line_index) => sdp_m_line_index,
            None => {
                gst::warning!(CAT, "No mandatory SDP m-line index");
                return;
            }
        };

        if let Some(session) = state.sessions.get(session_id) {
            gst::trace!(CAT, "adding ice candidate for session {}", session_id);
            session
                .webrtcbin
                .emit_by_name::<()>("add-ice-candidate", &[&sdp_m_line_index, &candidate]);
        } else {
            gst::warning!(CAT, "No consumer with ID {session_id}");
        }
    }

    fn handle_sdp_answer(
        &self,
        element: &super::BaseWebRTCSink,
        session_id: &str,
        desc: &gst_webrtc::WebRTCSessionDescription,
    ) {
        let mut state = self.state.lock().unwrap();

        if let Some(session) = state.sessions.get_mut(session_id) {
            let sdp = desc.sdp();

            session.sdp = Some(sdp.to_owned());

            for webrtc_pad in session.webrtc_pads.values_mut() {
                let media_idx = webrtc_pad.media_idx;
                /* TODO: support partial answer, webrtcbin doesn't seem
                 * very well equipped to deal with this at the moment */
                if let Some(media) = sdp.media(media_idx) {
                    if media.attribute_val("inactive").is_some() {
                        let media_str = sdp
                            .media(webrtc_pad.media_idx)
                            .and_then(|media| media.as_text().ok());

                        gst::warning!(
                            CAT,
                            "consumer from session {} refused media {}: {:?}",
                            session_id,
                            media_idx,
                            media_str
                        );
                        if let Some(_session) = state.end_session(session_id) {
                            drop(state);
                            let settings = self.settings.lock().unwrap();
                            let signaller = settings.signaller.clone();
                            drop(settings);
                            signaller.end_session(session_id);
                        }

                        gst::warning!(
                            CAT,
                            obj: element,
                            "Consumer refused media {session_id}, {media_idx}"
                        );
                        return;
                    }
                }

                if let Some(payload) = sdp
                    .media(webrtc_pad.media_idx)
                    .and_then(|media| media.format(0))
                    .and_then(|format| format.parse::<i32>().ok())
                {
                    webrtc_pad.payload = Some(payload);
                } else {
                    gst::warning!(
                        CAT,
                        "consumer from session {} did not provide valid payload for media index {} for session {}",
                        session_id,
                        media_idx,
                        session_id,
                    );

                    if let Some(_session) = state.end_session(session_id) {
                        drop(state);
                        let settings = self.settings.lock().unwrap();
                        let signaller = settings.signaller.clone();
                        drop(settings);
                        signaller.end_session(session_id);
                    }

                    gst::warning!(CAT, obj: element, "Consumer did not provide valid payload for media session: {session_id} media_ix: {media_idx}");
                    return;
                }
            }

            let element = element.downgrade();
            let session_id = session_id.to_string();

            let promise = gst::Promise::with_change_func(move |reply| {
                gst::debug!(CAT, "received reply {:?}", reply);
                if let Some(element) = element.upgrade() {
                    let this = element.imp();

                    this.on_remote_description_set(&element, session_id);
                }
            });

            session
                .webrtcbin
                .emit_by_name::<()>("set-remote-description", &[desc, &promise]);
        } else {
            gst::warning!(CAT, "No consumer with ID {session_id}");
        }
    }

    async fn run_discovery_pipeline(
        element: &super::BaseWebRTCSink,
        stream_name: &str,
        discovery_info: &DiscoveryInfo,
        codec: Codec,
        input_caps: gst::Caps,
        output_caps: &gst::Caps,
        twcc: Option<u32>,
    ) -> Result<gst::Structure, Error> {
        let pipe = PipelineWrapper(gst::Pipeline::default());

        let has_raw_input = is_raw_caps(&input_caps);
        let src = discovery_info.create_src();
        let mut elements = vec![src.clone().upcast::<gst::Element>()];
        let encoding_chain_src = if codec.is_video() && has_raw_input {
            elements.push(make_converter_for_video_caps(&input_caps, &codec)?);

            let capsfilter = make_element("capsfilter", Some("raw_capsfilter"))?;
            elements.push(capsfilter.clone());

            capsfilter
        } else {
            src.clone().upcast::<gst::Element>()
        };

        gst::debug!(
            CAT,
            obj: element,
            "Running discovery pipeline for input caps {input_caps} and output caps {output_caps} with codec {codec:?}"
        );

        gst::debug!(CAT, obj: element, "Running discovery pipeline");
        let elements_slice = &elements.iter().collect::<Vec<_>>();
        pipe.0.add_many(elements_slice).unwrap();
        gst::Element::link_many(elements_slice)
            .with_context(|| format!("Running discovery pipeline for caps {input_caps}"))?;

        let mut encoding_chain_builder = EncodingChainBuilder::new(
            &src.caps()
                .expect("Caps should always be set when starting discovery"),
            output_caps,
            &codec,
            element.emit_by_name::<Option<gst::Element>>(
                "request-encoded-filter",
                &[&Option::<String>::None, &stream_name, &codec.caps],
            ),
        );
        if let Some(twcc) = twcc {
            encoding_chain_builder = encoding_chain_builder.twcc(twcc)
        }
        let encoding_chain = encoding_chain_builder.build(&pipe.0, &encoding_chain_src)?;

        if let Some(ref enc) = encoding_chain.encoder {
            element.emit_by_name::<bool>(
                "encoder-setup",
                &[&"discovery".to_string(), &stream_name, &enc],
            );
        }

        let sink = gst_app::AppSink::builder()
            .callbacks(
                gst_app::AppSinkCallbacks::builder()
                    .new_event(|sink| {
                        let obj = sink.pull_object().ok();
                        if let Some(event) = obj.and_then(|o| o.downcast::<gst::Event>().ok()) {
                            if let gst::EventView::Caps(caps) = event.view() {
                                sink.post_message(gst::message::Application::new(
                                    gst::Structure::builder("payloaded_caps")
                                        .field("caps", &caps.caps().to_owned())
                                        .build(),
                                ))
                                .expect("Could not send message");
                            }
                        }

                        true
                    })
                    .build(),
            )
            .build();
        pipe.0.add(sink.upcast_ref::<gst::Element>()).unwrap();
        encoding_chain
            .pay_filter
            .link(&sink)
            .with_context(|| format!("Running discovery pipeline for caps {input_caps}"))?;

        let mut stream = pipe.0.bus().unwrap().stream();

        pipe.0
            .set_state(gst::State::Playing)
            .with_context(|| format!("Running discovery pipeline for caps {input_caps}"))?;

        while let Some(msg) = stream.next().await {
            match msg.view() {
                gst::MessageView::Error(err) => {
                    gst::error!(CAT, "Error in discovery pipeline: {err:#?}");
                    pipe.0.debug_to_dot_file_with_ts(
                        gst::DebugGraphDetails::all(),
                        "webrtcsink-discovery-error",
                    );
                    return Err(err.error().into());
                }
                gst::MessageView::StateChanged(s) => {
                    if msg.src() == Some(pipe.0.upcast_ref()) {
                        pipe.0.debug_to_dot_file_with_ts(
                            gst::DebugGraphDetails::all(),
                            format!(
                                "webrtcsink-discovery-{}-{:?}-{:?}",
                                pipe.0.name(),
                                s.old(),
                                s.current()
                            ),
                        );
                    }
                    continue;
                }
                gst::MessageView::Application(appmsg) => {
                    let caps = match appmsg.structure() {
                        Some(s) => {
                            if s.name().as_str() != "payloaded_caps" {
                                continue;
                            }

                            s.get::<gst::Caps>("caps").unwrap()
                        }
                        _ => continue,
                    };

                    gst::info!(CAT, "Discovery pipeline got caps {caps:?}");
                    pipe.0.debug_to_dot_file_with_ts(
                        gst::DebugGraphDetails::all(),
                        "webrtcsink-discovery-done",
                    );

                    if let Some(s) = caps.structure(0) {
                        let mut s = s.to_owned();
                        s.remove_fields([
                            "timestamp-offset",
                            "seqnum-offset",
                            "ssrc",
                            "sprop-parameter-sets",
                            "a-framerate",
                        ]);
                        s.set("payload", codec.payload().unwrap());
                        gst::debug!(
                            CAT,
                            obj: element,
                            "Codec discovery pipeline for caps {input_caps} with codec {codec:?} succeeded: {s}"
                        );
                        return Ok(s);
                    } else {
                        return Err(anyhow!("Discovered empty caps"));
                    }
                }
                _ => {
                    continue;
                }
            }
        }

        unreachable!()
    }

    async fn lookup_caps(
        element: &super::BaseWebRTCSink,
        discovery_info: DiscoveryInfo,
        name: String,
        output_caps: gst::Caps,
        codecs: &Codecs,
    ) -> Result<(), Error> {
        let futs = if let Some(codec) = codecs.find_for_encoded_caps(&discovery_info.caps) {
            let mut caps = discovery_info.caps.clone();

            gst::info!(
                CAT,
                obj: element,
                "Stream is already encoded with codec {}, still need to payload it",
                codec.name
            );

            caps = cleanup_codec_caps(caps);

            vec![BaseWebRTCSink::run_discovery_pipeline(
                element,
                &name,
                &discovery_info,
                codec,
                caps,
                &output_caps,
                Some(1),
            )]
        } else {
            let sink_caps = discovery_info.caps.clone();

            let is_video = match sink_caps.structure(0).unwrap().name().as_str() {
                "video/x-raw" => true,
                "audio/x-raw" => false,
                _ => unreachable!(),
            };

            codecs
                .iter()
                .filter(|codec| codec.is_video() == is_video)
                .map(|codec| {
                    BaseWebRTCSink::run_discovery_pipeline(
                        element,
                        &name,
                        &discovery_info,
                        codec.clone(),
                        sink_caps.clone(),
                        &output_caps,
                        Some(1),
                    )
                })
                .collect()
        };

        let mut payloader_caps = gst::Caps::new_empty();
        let payloader_caps_mut = payloader_caps.make_mut();

        for ret in futures::future::join_all(futs).await {
            match ret {
                Ok(s) => {
                    payloader_caps_mut.append_structure(s);
                }
                Err(err) => {
                    /* We don't consider this fatal, as long as we end up with one
                     * potential codec for each input stream
                     */
                    gst::warning!(
                        CAT,
                        obj: element,
                        "Codec discovery pipeline failed: {}",
                        err
                    );
                }
            }
        }

        let mut state = element.imp().state.lock().unwrap();
        if let Some(stream) = state.streams.get_mut(&name) {
            stream.out_caps = Some(payloader_caps.clone());
        }

        if payloader_caps.is_empty() {
            anyhow::bail!("No caps found for stream {name}");
        }

        Ok(())
    }

    fn gather_stats(&self) -> gst::Structure {
        gst::Structure::from_iter(
            "application/x-webrtcsink-stats",
            self.state
                .lock()
                .unwrap()
                .sessions
                .iter()
                .map(|(name, consumer)| (name.as_str(), consumer.gather_stats().to_send_value())),
        )
    }

    fn sink_event(
        &self,
        pad: &gst::Pad,
        element: &super::BaseWebRTCSink,
        event: gst::Event,
    ) -> bool {
        use gst::EventView;

        if let EventView::Caps(e) = event.view() {
            if let Some(caps) = pad.current_caps() {
                if caps.is_strictly_equal(e.caps()) {
                    // Nothing changed
                    return true;
                } else {
                    gst::error!(
                        CAT,
                        obj: pad,
                        "Renegotiation is not supported (old: {}, new: {})",
                        caps,
                        e.caps()
                    );
                    return false;
                }
            } else {
                gst::info!(CAT, obj: pad, "Received caps event {:?}", e);

                self.state
                    .lock()
                    .unwrap()
                    .streams
                    .iter_mut()
                    .for_each(|(_, stream)| {
                        if stream.sink_pad.upcast_ref::<gst::Pad>() == pad {
                            // We do not want VideoInfo to consider max-framerate
                            // when computing fps, so we strip it away here
                            let mut caps = e.caps().to_owned();
                            {
                                let mut_caps = caps.get_mut().unwrap();
                                if let Some(s) = mut_caps.structure_mut(0) {
                                    if s.has_name("video/x-raw") {
                                        s.remove_field("max-framerate");
                                    }
                                }
                            }
                            stream.in_caps = Some(caps.to_owned());
                        }
                    });
            }
        }

        gst::Pad::event_default(pad, Some(element), event)
    }

    fn start_stream_discovery_if_needed(&self, stream_name: &str, buffer: &gst::Buffer) {
        let (codecs, discovery_info) = {
            let mut state = self.state.lock().unwrap();
            let stream = state.streams.get_mut(stream_name).unwrap();

            // Discovery already happened... nothing to do here.
            if stream.out_caps.is_some() {
                return;
            }

            let mut discovery_started = false;
            for discovery_info in stream.discoveries.iter() {
                if matches!(discovery_info.type_, DiscoveryType::Initial) {
                    discovery_started = true;
                }
                for src in discovery_info.srcs() {
                    if let Err(err) = src.push_buffer(buffer.clone()) {
                        gst::log!(CAT, obj: src, "Failed to push buffer: {}", err);
                    }
                }
            }

            if discovery_started {
                // Discovery already started, we pushed the buffer to keep it
                // going
                return;
            }

            let discovery_info = stream.create_discovery(DiscoveryType::Initial);
            stream.discoveries.push(discovery_info.clone());

            let codecs = if !state.codecs.is_empty() {
                Codecs::from_map(&state.codecs)
            } else {
                let settings = self.settings.lock().unwrap();
                let codecs = Codecs::list_encoders(
                    settings.video_caps.iter().chain(settings.audio_caps.iter()),
                );

                state.codecs = codecs.to_map();

                codecs
            };

            (codecs, discovery_info)
        };

        let stream_name_clone = stream_name.to_owned();
        RUNTIME.spawn(glib::clone!(@weak self as this, @strong discovery_info => async move {
            let element = &*this.obj();
            let (fut, handle) = futures::future::abortable(
                Self::lookup_caps(
                    element,
                    discovery_info,
                    stream_name_clone,
                    gst::Caps::new_any(),
                    &codecs,
                ));

            let (codecs_done_sender, codecs_done_receiver) =
                futures::channel::oneshot::channel();

            // Compiler isn't budged by dropping state before await,
            // so let's make a new scope instead.
            {
                let mut state = this.state.lock().unwrap();
                state.codecs_abort_handles.push(handle);
                state.codecs_done_receivers.push(codecs_done_receiver);
            }

            match fut.await {
                Ok(Err(err)) => {
                    gst::error!(CAT, imp: this, "Error running discovery: {err:?}");
                    gst::element_error!(
                        this.obj(),
                        gst::StreamError::CodecNotFound,
                        ["Failed to look up output caps: {err:?}"]
                    );
                }
                Ok(Ok(_)) => {
                    let settings = this.settings.lock().unwrap();
                    let mut state = this.state.lock().unwrap();
                    state.codec_discovery_done = state.streams.values().all(|stream| stream.out_caps.is_some());
                    let signaller = settings.signaller.clone();
                    drop(settings);
                    if state.should_start_signaller(element) {
                        state.signaller_state = SignallerState::Started;
                        drop(state);
                        signaller.start();
                    }
                }
                _ => (),
            }

            let _ = codecs_done_sender.send(());
        }));

        let mut state = self.state.lock().unwrap();
        let stream = state.streams.get_mut(stream_name).unwrap();
        stream.remove_discovery(&discovery_info);
    }

    fn chain(
        &self,
        pad: &gst::GhostPad,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        self.start_stream_discovery_if_needed(pad.name().as_str(), &buffer);

        gst::ProxyPad::chain_default(pad, Some(&*self.obj()), buffer)
    }
}

#[glib::object_subclass]
impl ObjectSubclass for BaseWebRTCSink {
    const NAME: &'static str = "GstBaseWebRTCSink";
    type Type = super::BaseWebRTCSink;
    type ParentType = gst::Bin;
    type Interfaces = (gst::ChildProxy, gst_video::Navigation);
}

unsafe impl<T: BaseWebRTCSinkImpl> IsSubclassable<T> for super::BaseWebRTCSink {
    fn class_init(class: &mut glib::Class<Self>) {
        Self::parent_class_init::<T>(class);
    }
}

pub(crate) trait BaseWebRTCSinkImpl: BinImpl {}

impl ObjectImpl for BaseWebRTCSink {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                glib::ParamSpecBoxed::builder::<gst::Caps>("video-caps")
                    .nick("Video encoder caps")
                    .blurb("Governs what video codecs will be proposed")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoxed::builder::<gst::Caps>("audio-caps")
                    .nick("Audio encoder caps")
                    .blurb("Governs what audio codecs will be proposed")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("stun-server")
                    .nick("STUN Server")
                    .blurb("The STUN server of the form stun://hostname:port")
                    .default_value(DEFAULT_STUN_SERVER)
                    .build(),
                gst::ParamSpecArray::builder("turn-servers")
                    .nick("List of TURN Servers to user")
                    .blurb("The TURN servers of the form <\"turn(s)://username:password@host:port\", \"turn(s)://username1:password1@host1:port1\">")
                    .element_spec(&glib::ParamSpecString::builder("turn-server")
                        .nick("TURN Server")
                        .blurb("The TURN server of the form turn(s)://username:password@host:port.")
                        .build()
                    )
                    .mutable_ready()
                    .build(),
                glib::ParamSpecEnum::builder_with_default("congestion-control", DEFAULT_CONGESTION_CONTROL)
                    .nick("Congestion control")
                    .blurb("Defines how congestion is controlled, if at all")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt::builder("min-bitrate")
                    .nick("Minimal Bitrate")
                    .blurb("Minimal bitrate to use (in bit/sec) when computing it through the congestion control algorithm")
                    .minimum(1)
                    .maximum(u32::MAX)
                    .default_value(DEFAULT_MIN_BITRATE)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt::builder("max-bitrate")
                    .nick("Maximum Bitrate")
                    .blurb("Maximum bitrate to use (in bit/sec) when computing it through the congestion control algorithm")
                    .minimum(1)
                    .maximum(u32::MAX)
                    .default_value(DEFAULT_MAX_BITRATE)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt::builder("start-bitrate")
                    .nick("Start Bitrate")
                    .blurb("Start bitrate to use (in bit/sec)")
                    .minimum(1)
                    .maximum(u32::MAX)
                    .default_value(DEFAULT_START_BITRATE)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoxed::builder::<gst::Structure>("stats")
                    .nick("Consumer statistics")
                    .blurb("Statistics for the current consumers")
                    .read_only()
                    .build(),
                glib::ParamSpecBoolean::builder("do-fec")
                    .nick("Do Forward Error Correction")
                    .blurb("Whether the element should negotiate and send FEC data")
                    .default_value(DEFAULT_DO_FEC)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoolean::builder("do-retransmission")
                    .nick("Do retransmission")
                    .blurb("Whether the element should offer to honor retransmission requests")
                    .default_value(DEFAULT_DO_RETRANSMISSION)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoolean::builder("enable-data-channel-navigation")
                    .nick("Enable data channel navigation")
                    .blurb("Enable navigation events through a dedicated WebRTCDataChannel")
                    .default_value(DEFAULT_ENABLE_DATA_CHANNEL_NAVIGATION)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoxed::builder::<gst::Structure>("meta")
                    .nick("Meta")
                    .blurb("Free form metadata about the producer")
                    .build(),
                glib::ParamSpecEnum::builder_with_default("ice-transport-policy", DEFAULT_ICE_TRANSPORT_POLICY)
                    .nick("ICE Transport Policy")
                    .blurb("The policy to apply for ICE transport")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecObject::builder::<Signallable>("signaller")
                    .flags(glib::ParamFlags::READABLE  | gst::PARAM_FLAG_MUTABLE_READY)
                    .blurb("The Signallable object to use to handle WebRTC Signalling")
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "video-caps" => {
                let mut settings = self.settings.lock().unwrap();
                settings.video_caps = value
                    .get::<Option<gst::Caps>>()
                    .expect("type checked upstream")
                    .unwrap_or_else(gst::Caps::new_empty);
            }
            "audio-caps" => {
                let mut settings = self.settings.lock().unwrap();
                settings.audio_caps = value
                    .get::<Option<gst::Caps>>()
                    .expect("type checked upstream")
                    .unwrap_or_else(gst::Caps::new_empty);
            }
            "stun-server" => {
                let mut settings = self.settings.lock().unwrap();
                settings.stun_server = value
                    .get::<Option<String>>()
                    .expect("type checked upstream")
            }
            "turn-servers" => {
                let mut settings = self.settings.lock().unwrap();
                settings.turn_servers = value.get::<gst::Array>().expect("type checked upstream")
            }
            "congestion-control" => {
                let mut settings = self.settings.lock().unwrap();
                settings.cc_info.heuristic = value
                    .get::<WebRTCSinkCongestionControl>()
                    .expect("type checked upstream");
            }
            "min-bitrate" => {
                let mut settings = self.settings.lock().unwrap();
                settings.cc_info.min_bitrate = value.get::<u32>().expect("type checked upstream");
            }
            "max-bitrate" => {
                let mut settings = self.settings.lock().unwrap();
                settings.cc_info.max_bitrate = value.get::<u32>().expect("type checked upstream");
            }
            "start-bitrate" => {
                let mut settings = self.settings.lock().unwrap();
                settings.cc_info.start_bitrate = value.get::<u32>().expect("type checked upstream");
            }
            "do-fec" => {
                let mut settings = self.settings.lock().unwrap();
                settings.do_fec = value.get::<bool>().expect("type checked upstream");
            }
            "do-retransmission" => {
                let mut settings = self.settings.lock().unwrap();
                settings.do_retransmission = value.get::<bool>().expect("type checked upstream");
            }
            "enable-data-channel-navigation" => {
                let mut settings = self.settings.lock().unwrap();
                settings.enable_data_channel_navigation =
                    value.get::<bool>().expect("type checked upstream");
            }
            "meta" => {
                let mut settings = self.settings.lock().unwrap();
                settings.meta = value
                    .get::<Option<gst::Structure>>()
                    .expect("type checked upstream")
            }
            "ice-transport-policy" => {
                let mut settings = self.settings.lock().unwrap();
                settings.ice_transport_policy = value
                    .get::<WebRTCICETransportPolicy>()
                    .expect("type checked upstream");
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "video-caps" => {
                let settings = self.settings.lock().unwrap();
                settings.video_caps.to_value()
            }
            "audio-caps" => {
                let settings = self.settings.lock().unwrap();
                settings.audio_caps.to_value()
            }
            "congestion-control" => {
                let settings = self.settings.lock().unwrap();
                settings.cc_info.heuristic.to_value()
            }
            "stun-server" => {
                let settings = self.settings.lock().unwrap();
                settings.stun_server.to_value()
            }
            "turn-servers" => {
                let settings = self.settings.lock().unwrap();
                settings.turn_servers.to_value()
            }
            "min-bitrate" => {
                let settings = self.settings.lock().unwrap();
                settings.cc_info.min_bitrate.to_value()
            }
            "max-bitrate" => {
                let settings = self.settings.lock().unwrap();
                settings.cc_info.max_bitrate.to_value()
            }
            "start-bitrate" => {
                let settings = self.settings.lock().unwrap();
                settings.cc_info.start_bitrate.to_value()
            }
            "do-fec" => {
                let settings = self.settings.lock().unwrap();
                settings.do_fec.to_value()
            }
            "do-retransmission" => {
                let settings = self.settings.lock().unwrap();
                settings.do_retransmission.to_value()
            }
            "enable-data-channel-navigation" => {
                let settings = self.settings.lock().unwrap();
                settings.enable_data_channel_navigation.to_value()
            }
            "stats" => self.gather_stats().to_value(),
            "meta" => {
                let settings = self.settings.lock().unwrap();
                settings.meta.to_value()
            }
            "ice-transport-policy" => {
                let settings = self.settings.lock().unwrap();
                settings.ice_transport_policy.to_value()
            }
            "signaller" => self.settings.lock().unwrap().signaller.to_value(),
            _ => unimplemented!(),
        }
    }

    fn signals() -> &'static [glib::subclass::Signal] {
        static SIGNALS: Lazy<Vec<glib::subclass::Signal>> = Lazy::new(|| {
            vec![
                /**
                 * RsBaseWebRTCSink::consumer-added:
                 * @consumer_id: Identifier of the consumer added
                 * @webrtcbin: The new webrtcbin
                 *
                 * This signal can be used to tweak @webrtcbin, creating a data
                 * channel for example.
                 */
                glib::subclass::Signal::builder("consumer-added")
                    .param_types([String::static_type(), gst::Element::static_type()])
                    .build(),
                /**
                 * RsBaseWebRTCSink::consumer-pipeline-created:
                 * @consumer_id: Identifier of the consumer
                 * @pipeline: The pipeline that was just created
                 *
                 * This signal is emitted right after the pipeline for a new consumer
                 * has been created, for instance allowing handlers to connect to
                 * #GstBin::deep-element-added and tweak properties of any element used
                 * by the pipeline.
                 *
                 * This provides access to the lower level components of webrtcsink, and
                 * no guarantee is made that its internals will remain stable, use with caution!
                 *
                 * This is emitted *before* #RsBaseWebRTCSink::consumer-added .
                 */
                glib::subclass::Signal::builder("consumer-pipeline-created")
                    .param_types([String::static_type(), gst::Pipeline::static_type()])
                    .build(),
                /**
                 * RsBaseWebRTCSink::consumer_removed:
                 * @consumer_id: Identifier of the consumer that was removed
                 * @webrtcbin: The webrtcbin connected to the newly removed consumer
                 *
                 * This signal is emitted right after the connection with a consumer
                 * has been dropped.
                 */
                glib::subclass::Signal::builder("consumer-removed")
                    .param_types([String::static_type(), gst::Element::static_type()])
                    .build(),
                /**
                 * RsBaseWebRTCSink::get_sessions:
                 *
                 * List all sessions (by ID).
                 */
                glib::subclass::Signal::builder("get-sessions")
                    .action()
                    .class_handler(|_, args| {
                        let element = args[0].get::<super::BaseWebRTCSink>().expect("signal arg");
                        let this = element.imp();

                        let res = Some(
                            this.state
                                .lock()
                                .unwrap()
                                .sessions
                                .keys()
                                .cloned()
                                .collect::<Vec<String>>()
                                .to_value(),
                        );
                        res
                    })
                    .return_type::<Vec<String>>()
                    .build(),
                /**
                 * RsBaseWebRTCSink::encoder-setup:
                 * @consumer_id: Identifier of the consumer, or "discovery"
                 *   when the encoder is used in a discovery pipeline.
                 * @pad_name: The name of the corresponding input pad
                 * @encoder: The constructed encoder
                 *
                 * This signal can be used to tweak @encoder properties.
                 *
                 * Returns: True if the encoder is entirely configured,
                 * False to let other handlers run
                 */
                glib::subclass::Signal::builder("encoder-setup")
                    .param_types([
                        String::static_type(),
                        String::static_type(),
                        gst::Element::static_type(),
                    ])
                    .return_type::<bool>()
                    .accumulator(|_hint, _ret, value| !value.get::<bool>().unwrap())
                    .class_handler(|_, args| {
                        let element = args[0].get::<super::BaseWebRTCSink>().expect("signal arg");
                        let enc = args[3].get::<gst::Element>().unwrap();

                        gst::debug!(
                            CAT,
                            obj: element,
                            "applying default configuration on encoder {:?}",
                            enc
                        );

                        let this = element.imp();
                        let settings = this.settings.lock().unwrap();
                        configure_encoder(&enc, settings.cc_info.start_bitrate);

                        // Return false here so that latter handlers get called
                        Some(false.to_value())
                    })
                    .build(),
                /**
                 * RsWebRTCSink::request-encoded-filter:
                 * @consumer_id: Identifier of the consumer
                 * @pad_name: The name of the corresponding input pad
                 * @encoded_caps: The Caps of the encoded stream
                 *
                 * This signal can be used to insert a filter
                 * element between the encoder and the payloader.
                 *
                 * When called during Caps discovery, the `consumer_id` is `None`.
                 *
                 * Returns: the element to insert.
                 */
                glib::subclass::Signal::builder("request-encoded-filter")
                    .param_types([
                        Option::<String>::static_type(),
                        String::static_type(),
                        gst::Caps::static_type(),
                    ])
                    .return_type::<gst::Element>()
                    .build(),
            ]
        });

        SIGNALS.as_ref()
    }

    fn constructed(&self) {
        self.parent_constructed();
        let signaller = self.settings.lock().unwrap().signaller.clone();

        self.connect_signaller(&signaller);

        let obj = self.obj();
        obj.set_suppressed_flags(gst::ElementFlags::SINK | gst::ElementFlags::SOURCE);
        obj.set_element_flags(gst::ElementFlags::SINK);
    }
}

impl GstObjectImpl for BaseWebRTCSink {}

impl ElementImpl for BaseWebRTCSink {
    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let mut caps_builder = gst::Caps::builder_full()
                .structure(gst::Structure::builder("video/x-raw").build())
                .structure_with_features(
                    gst::Structure::builder("video/x-raw").build(),
                    gst::CapsFeatures::new([CUDA_MEMORY_FEATURE]),
                )
                .structure_with_features(
                    gst::Structure::builder("video/x-raw").build(),
                    gst::CapsFeatures::new([GL_MEMORY_FEATURE]),
                )
                .structure_with_features(
                    gst::Structure::builder("video/x-raw").build(),
                    gst::CapsFeatures::new([NVMM_MEMORY_FEATURE]),
                );

            for codec in Codecs::video_codecs() {
                caps_builder = caps_builder.structure(codec.caps.structure(0).unwrap().to_owned());
            }

            let video_pad_template = gst::PadTemplate::new(
                "video_%u",
                gst::PadDirection::Sink,
                gst::PadPresence::Request,
                &caps_builder.build(),
            )
            .unwrap();

            let mut caps_builder =
                gst::Caps::builder_full().structure(gst::Structure::builder("audio/x-raw").build());
            for codec in Codecs::audio_codecs() {
                caps_builder = caps_builder.structure(codec.caps.structure(0).unwrap().to_owned());
            }
            let audio_pad_template = gst::PadTemplate::new(
                "audio_%u",
                gst::PadDirection::Sink,
                gst::PadPresence::Request,
                &caps_builder.build(),
            )
            .unwrap();

            vec![video_pad_template, audio_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }

    fn request_new_pad(
        &self,
        templ: &gst::PadTemplate,
        _name: Option<&str>,
        _caps: Option<&gst::Caps>,
    ) -> Option<gst::Pad> {
        let element = self.obj();
        if element.current_state() > gst::State::Ready {
            gst::error!(CAT, "element pads can only be requested before starting");
            return None;
        }

        let mut state = self.state.lock().unwrap();

        let serial;

        let (name, is_video) = if templ.name().starts_with("video_") {
            let name = format!("video_{}", state.video_serial);
            serial = state.video_serial;
            state.video_serial += 1;

            (name, true)
        } else {
            let name = format!("audio_{}", state.audio_serial);
            serial = state.audio_serial;
            state.audio_serial += 1;
            (name, false)
        };

        let sink_pad = gst::GhostPad::builder_from_template(templ)
            .name(name.as_str())
            .chain_function(|pad, parent, buffer| {
                BaseWebRTCSink::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |this| this.chain(pad, buffer),
                )
            })
            .event_function(|pad, parent, event| {
                BaseWebRTCSink::catch_panic_pad_function(
                    parent,
                    || false,
                    |this| this.sink_event(pad.upcast_ref(), &this.obj(), event),
                )
            })
            .build();

        sink_pad.set_active(true).unwrap();
        sink_pad.use_fixed_caps();
        element.add_pad(&sink_pad).unwrap();

        state.streams.insert(
            name,
            InputStream {
                sink_pad: sink_pad.clone(),
                producer: None,
                in_caps: None,
                out_caps: None,
                clocksync: None,
                is_video,
                serial,
                discoveries: Default::default(),
            },
        );

        Some(sink_pad.upcast())
    }

    fn change_state(
        &self,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        let element = self.obj();
        if let gst::StateChange::ReadyToPaused = transition {
            if let Err(err) = self.prepare(&element) {
                gst::element_error!(
                    element,
                    gst::StreamError::Failed,
                    ["Failed to prepare: {}", err]
                );
                return Err(gst::StateChangeError);
            }
        }

        let mut ret = self.parent_change_state(transition);

        match transition {
            gst::StateChange::PausedToReady => {
                let unprepare_res = match tokio::runtime::Handle::try_current() {
                    Ok(_) => {
                        gst::error!(
                            CAT,
                            obj: element,
                            "Trying to set state to NULL from an async \
                                    tokio context, working around the panic but \
                                    you should refactor your code to make use of \
                                    gst::Element::call_async and set the state to \
                                    NULL from there, without blocking the runtime"
                        );
                        let (tx, rx) = mpsc::channel();
                        element.call_async(move |element| {
                            tx.send(element.imp().unprepare(element)).unwrap();
                        });
                        rx.recv().unwrap()
                    }
                    Err(_) => self.unprepare(&element),
                };

                if let Err(err) = unprepare_res {
                    gst::element_error!(
                        element,
                        gst::StreamError::Failed,
                        ["Failed to unprepare: {}", err]
                    );
                    return Err(gst::StateChangeError);
                }
            }
            gst::StateChange::ReadyToPaused => {
                ret = Ok(gst::StateChangeSuccess::NoPreroll);
            }
            gst::StateChange::PausedToPlaying => {
                let settings = self.settings.lock().unwrap();
                let signaller = settings.signaller.clone();
                drop(settings);
                let mut state = self.state.lock().unwrap();
                if state.should_start_signaller(&element) {
                    state.signaller_state = SignallerState::Started;
                    drop(state);
                    signaller.start();
                }
            }
            _ => (),
        }

        ret
    }
}

impl BinImpl for BaseWebRTCSink {}

impl ChildProxyImpl for BaseWebRTCSink {
    fn child_by_index(&self, _index: u32) -> Option<glib::Object> {
        None
    }

    fn children_count(&self) -> u32 {
        0
    }

    fn child_by_name(&self, name: &str) -> Option<glib::Object> {
        match name {
            "signaller" => Some(self.settings.lock().unwrap().signaller.clone().upcast()),
            _ => None,
        }
    }
}

impl NavigationImpl for BaseWebRTCSink {
    fn send_event(&self, event_def: gst::Structure) {
        let mut state = self.state.lock().unwrap();
        let event = gst::event::Navigation::new(event_def);

        state.streams.iter_mut().for_each(|(_, stream)| {
            if stream.sink_pad.name().starts_with("video_") {
                gst::log!(CAT, "Navigating to: {:?}", event);
                // FIXME: Handle multi tracks.
                if !stream.sink_pad.push_event(event.clone()) {
                    gst::info!(CAT, "Could not send event: {:?}", event);
                }
            }
        });
    }
}

#[derive(Default)]
pub struct WebRTCSink {}

impl ObjectImpl for WebRTCSink {}

impl GstObjectImpl for WebRTCSink {}

impl ElementImpl for WebRTCSink {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "WebRTCSink",
                "Sink/Network/WebRTC",
                "WebRTC sink with custom protocol signaller",
                "Mathieu Duponchelle <mathieu@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }
}

impl BinImpl for WebRTCSink {}

impl BaseWebRTCSinkImpl for WebRTCSink {}

#[glib::object_subclass]
impl ObjectSubclass for WebRTCSink {
    const NAME: &'static str = "GstWebRTCSink";
    type Type = super::WebRTCSink;
    type ParentType = super::BaseWebRTCSink;
}

#[derive(Default)]
pub struct AwsKvsWebRTCSink {}

impl ObjectImpl for AwsKvsWebRTCSink {
    fn constructed(&self) {
        let element = self.obj();
        let ws = element.upcast_ref::<super::BaseWebRTCSink>().imp();

        let _ = ws.set_signaller(AwsKvsSignaller::default().upcast());
    }
}

impl GstObjectImpl for AwsKvsWebRTCSink {}

impl ElementImpl for AwsKvsWebRTCSink {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "AwsKvsWebRTCSink",
                "Sink/Network/WebRTC",
                "WebRTC sink with kinesis video streams signaller",
                "Mathieu Duponchelle <mathieu@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }
}

impl BinImpl for AwsKvsWebRTCSink {}

impl BaseWebRTCSinkImpl for AwsKvsWebRTCSink {}

#[glib::object_subclass]
impl ObjectSubclass for AwsKvsWebRTCSink {
    const NAME: &'static str = "GstAwsKvsWebRTCSink";
    type Type = super::AwsKvsWebRTCSink;
    type ParentType = super::BaseWebRTCSink;
}

#[derive(Default)]
pub struct WhipWebRTCSink {}

impl ObjectImpl for WhipWebRTCSink {
    fn constructed(&self) {
        let element = self.obj();
        let ws = element.upcast_ref::<super::BaseWebRTCSink>().imp();

        let _ = ws.set_signaller(WhipSignaller::default().upcast());
    }
}

impl GstObjectImpl for WhipWebRTCSink {}

impl ElementImpl for WhipWebRTCSink {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "WhipWebRTCSink",
                "Sink/Network/WebRTC",
                "WebRTC sink with WHIP client signaller",
                "Taruntej Kanakamalla <taruntej@asymptotic.io>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }
}

impl BinImpl for WhipWebRTCSink {}

impl BaseWebRTCSinkImpl for WhipWebRTCSink {}

#[glib::object_subclass]
impl ObjectSubclass for WhipWebRTCSink {
    const NAME: &'static str = "GstWhipWebRTCSink";
    type Type = super::WhipWebRTCSink;
    type ParentType = super::BaseWebRTCSink;
}

#[derive(Default)]
pub struct LiveKitWebRTCSink {}

impl ObjectImpl for LiveKitWebRTCSink {
    fn constructed(&self) {
        let element = self.obj();
        let ws = element.upcast_ref::<super::BaseWebRTCSink>().imp();

        let _ = ws.set_signaller(LiveKitSignaller::default().upcast());
    }
}

impl GstObjectImpl for LiveKitWebRTCSink {}

impl ElementImpl for LiveKitWebRTCSink {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "LiveKitWebRTCSink",
                "Sink/Network/WebRTC",
                "WebRTC sink with LiveKit signaller",
                "Olivier Crête <olivier.crete@collabora.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }
}

impl BinImpl for LiveKitWebRTCSink {}

impl BaseWebRTCSinkImpl for LiveKitWebRTCSink {}

#[glib::object_subclass]
impl ObjectSubclass for LiveKitWebRTCSink {
    const NAME: &'static str = "GstLiveKitWebRTCSink";
    type Type = super::LiveKitWebRTCSink;
    type ParentType = super::BaseWebRTCSink;
}
