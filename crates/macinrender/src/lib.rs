//! Safe owner of the `MacinRender` C ABI. Only `Session` can produce Scene frames;
//! cloned controls retain its handles and serialize calls/error-string borrowing.
#![allow(clippy::missing_errors_doc, clippy::missing_panics_doc)]

// ADM Core implements its C ABI in Rust. Link that crate rather than its
// staticlib so Cargo and ThinLTO share one standard library with the player;
// `raw` declares the `adm_*` exports it provides.
#[cfg(native_macinrender)]
extern crate mradm_capi as _;

mod api;
#[cfg(target_os = "macos")]
pub mod atmos;
#[cfg(all(test, native_macinrender, unix))]
mod hptf_tests;
pub mod motion;
#[cfg(test)]
#[path = "../native_link.rs"]
#[allow(
    dead_code,
    reason = "Exercise the build script link parser in workspace tests"
)]
mod native_link;
mod raw;
#[cfg(all(test, native_macinrender))]
mod room_tests;

use std::ffi::{CStr, CString, c_char, c_void};
use std::ptr::NonNull;
use std::sync::{Arc, Mutex};

use api::Api;

/// `ADM_ERROR_UNSUPPORTED`: the call is meaningless for this output rather
/// than failed.
const UNSUPPORTED: i32 = 2;

fn size<T>() -> u32 {
    u32::try_from(size_of::<T>()).expect("C ABI structure exceeds u32")
}
fn string(value: &str) -> Result<CString, String> {
    CString::new(value).map_err(|_| "Native text contains NUL".into())
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SpeakerRenderer {
    #[default]
    SafVbap,
    TripleBalance,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RendererSettings {
    pub binaural: bool,
    pub speaker_renderer: SpeakerRenderer,
    pub layout: String,
    pub sofa: String,
    pub split_lfe: bool,
    /// The room renderer forms the binaural feed instead of SAF HRTF. Ignored
    /// on a speaker output.
    pub room: Option<RoomSettings>,
}

/// What a room session is created with (`adm_room_config_t` and the screens).
///
/// The renderer fixes everything here but the room package itself when the
/// session is created: switching to another room keeps the HRIR, the scale
/// and the screens.
#[derive(Debug, Clone)]
pub struct RoomSettings {
    /// The room package's `room.json`.
    pub manifest: String,
    /// A SOFA file, a prepared HRIR package or a collection; empty listens
    /// through the HRIR the room package binds.
    pub hrir: String,
    /// Metres per unit of Scene distance; `None` maps each axis of the room
    /// from the listener to its walls, which is the renderer's own default.
    pub metres_per_unit: Option<f64>,
    /// `mradm.screen-config` v1; `None` leaves every position unchanged.
    pub screens: Option<String>,
}

impl PartialEq for RoomSettings {
    fn eq(&self, other: &Self) -> bool {
        self.manifest == other.manifest
            && self.hrir == other.hrir
            && self.metres_per_unit.map(f64::to_bits) == other.metres_per_unit.map(f64::to_bits)
            && self.screens == other.screens
    }
}
impl Eq for RoomSettings {}

/// A validated room package. Loading and verifying its files uses its own
/// context, so it can run independently of any live stream.
pub struct RoomPackage {
    api: Api,
    handle: NonNull<c_void>,
}

// SAFETY: the native handle owns immutable package data in an Arc and has no
// thread affinity. Moving the handle transfers its one destruction obligation.
unsafe impl Send for RoomPackage {}

impl RoomPackage {
    pub fn load(path: &str) -> Result<Self, String> {
        let api = Api::load()?;
        let manifest = string(path)?;
        // SAFETY: this context is private to this call.
        let context = unsafe { (api.adm_create_context)() };
        if context.is_null() {
            return Err("Cannot create MacinRender room context".into());
        }
        let mut handle = std::ptr::null_mut();
        // SAFETY: the context and manifest live through the call, which writes
        // a handle only on success.
        let code = unsafe { (api.adm_room_load)(context, manifest.as_ptr(), &raw mut handle) };
        let error = (code != 0).then(|| {
            // SAFETY: the context still owns the borrowed error string.
            copy_text(unsafe { (api.adm_context_last_error_message)(context) })
        });
        // SAFETY: the returned room's lifetime is independent of the context.
        unsafe { (api.adm_destroy_context)(context) };
        if let Some(handle) = NonNull::new(handle) {
            return Ok(Self { api, handle });
        }
        let error = error
            .filter(|error| !error.is_empty())
            .unwrap_or_else(|| format!("MacinRender error {code}"));
        Err(format!("Cannot load the room package {path}: {error}"))
    }

    fn manifest_json(&self) -> String {
        // SAFETY: the manifest is borrowed from the live room handle.
        copy_text(unsafe { (self.api.adm_room_manifest_json)(self.handle.as_ptr()) })
    }

    fn with_config<T>(
        &self,
        settings: &RoomSettings,
        action: impl FnOnce(&raw::RoomConfig) -> T,
    ) -> Result<T, String> {
        let hrir = string(&settings.hrir)?;
        Ok(action(&raw::RoomConfig {
            size: size::<raw::RoomConfig>(),
            reserved: 0,
            room: self.handle.as_ptr(),
            hrir: if settings.hrir.is_empty() {
                std::ptr::null()
            } else {
                hrir.as_ptr()
            },
            metres_per_unit: settings.metres_per_unit.unwrap_or(0.0),
        }))
    }
}

impl Drop for RoomPackage {
    fn drop(&mut self) {
        // SAFETY: this handle was transferred by adm_room_load and is owned here.
        unsafe { (self.api.adm_room_destroy)(self.handle.as_ptr()) };
    }
}

/// `ADM_ROOM_QUALITY_REALTIME`: native SIMD FFTs and a staggered reflection
/// grid (`room-realtime-v2`). The exact tier keeps the cross-platform bit
/// contract offline rendering needs; playback needs the deadline instead,
/// and it is the tier the renderer's device soak tests run.
const ROOM_QUALITY_REALTIME: i32 = 1;

/// Where a room change stands (`mradm.room-status` v1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoomState {
    Applied,
    Preparing,
    Transitioning,
    Draining,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomStatus {
    pub state: RoomState,
    pub requested_revision: u64,
    pub applied_revision: u64,
    /// Sources the walls are holding inside the room this instant.
    pub constrained_sources: u64,
    pub message: String,
}

impl RoomStatus {
    fn parse(json: &str) -> Result<Self, String> {
        let value: serde_json::Value =
            serde_json::from_str(json).map_err(|error| format!("Room status: {error}"))?;
        let number = |key: &str| {
            value
                .get(key)
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0)
        };
        Ok(Self {
            state: match value.get("state").and_then(serde_json::Value::as_str) {
                Some("applied") => RoomState::Applied,
                Some("preparing") => RoomState::Preparing,
                Some("transitioning") => RoomState::Transitioning,
                Some("draining") => RoomState::Draining,
                Some("failed") => RoomState::Failed,
                other => return Err(format!("Unknown room state {other:?}")),
            },
            requested_revision: number("requested_revision"),
            applied_revision: number("applied_revision"),
            constrained_sources: number("constrained_sources"),
            message: value
                .get("message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        })
    }
}

/// Where the room renderer last placed one source, after pulling it inside
/// the walls: the position it is heard from.
///
/// A snapshot the render worker takes after each block it renders, at that
/// block's end. That is ahead of what is being heard by the output's queue,
/// and the renderer does not say by how much.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RoomSource {
    /// Room coordinates in metres from the corner: x front to back, y right
    /// to left, z up. A head-locked source's are in the head's frame.
    pub position: [f64; 3],
    /// Unit direction from the listener, in the same axes.
    pub direction: [f64; 3],
    pub distance: f64,
    /// Whether a wall or the minimum distance moved it from where the Scene
    /// asked.
    pub constrained: bool,
}

impl RoomSource {
    /// `None` for a source the renderer has not placed: unknown to it,
    /// inactive, or not yet rendered.
    fn parse(json: &str) -> Result<Option<Self>, String> {
        let value: serde_json::Value =
            serde_json::from_str(json).map_err(|error| format!("Room source: {error}"))?;
        if value.get("present").and_then(serde_json::Value::as_bool) != Some(true) {
            return Ok(None);
        }
        let triple = |key: &str| -> Result<[f64; 3], String> {
            let values = value
                .get(key)
                .and_then(serde_json::Value::as_array)
                .filter(|values| values.len() == 3)
                .ok_or_else(|| format!("Room source: no {key}"))?;
            let mut out = [0.0; 3];
            for (slot, value) in out.iter_mut().zip(values) {
                *slot = value
                    .as_f64()
                    .ok_or_else(|| format!("Room source: {key} is not numeric"))?;
            }
            Ok(out)
        };
        Ok(Some(Self {
            position: triple("position_metres")?,
            direction: triple("direction")?,
            distance: value
                .get("distance_metres")
                .and_then(serde_json::Value::as_f64)
                .ok_or("Room source: no distance")?,
            constrained: value
                .get("constrained")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
        }))
    }
}

impl RendererSettings {
    fn room(&self) -> Option<&RoomSettings> {
        self.room.as_ref().filter(|_| self.binaural)
    }

    const fn geometry(&self) -> i32 {
        if !self.binaural && matches!(self.speaker_renderer, SpeakerRenderer::TripleBalance) {
            0
        } else {
            1
        }
    }

    fn with_raw<T>(&self, action: impl FnOnce(&raw::RendererConfig) -> T) -> Result<T, String> {
        let layout = string(if self.binaural {
            "binaural"
        } else {
            &self.layout
        })?;
        // The room takes its HRIR through its own configuration.
        let sofa = string(if self.room().is_some() {
            ""
        } else {
            &self.sofa
        })?;
        Ok(action(&raw::RendererConfig {
            size: size::<raw::RendererConfig>(),
            renderer: if self.room().is_some() {
                8
            } else if self.binaural {
                6
            } else {
                match self.speaker_renderer {
                    SpeakerRenderer::SafVbap => 2,
                    SpeakerRenderer::TripleBalance => 7,
                }
            },
            layout: layout.as_ptr(),
            sofa: sofa.as_ptr(),
            geometry: self.geometry(),
            lfe: i32::from(self.split_lfe && self.layout == "9+10+3" && !self.binaural),
            ..Default::default()
        }))
    }
}

/// `AutoEq` `ParametricEQ.txt` headphone compensation. An empty profile disables
/// it, which the renderer treats as an exact short-circuit rather than a
/// unit-gain multiply.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HptfSettings {
    pub profile: String,
    /// Attenuate further when the designed cascade still peaks above 0 dB across
    /// 20 Hz-20 kHz. Off means the file's own `Preamp:` line is used verbatim.
    pub auto_trim: bool,
}

/// The filter shapes a `ParametricEQ` band can name. The C enumeration's
/// integers are mapped by hand rather than transmuted: this is the one place
/// that knows them, and a reordering upstream has to fail here loudly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HptfBandKind {
    Peaking,
    LowShelf,
    HighShelf,
    LowPass,
    HighPass,
    BandPass,
    Notch,
}

impl HptfBandKind {
    fn of(kind: i32) -> Result<Self, String> {
        Ok(match kind {
            0 => Self::Peaking,
            1 => Self::LowShelf,
            2 => Self::HighShelf,
            3 => Self::LowPass,
            4 => Self::HighPass,
            5 => Self::BandPass,
            6 => Self::Notch,
            other => return Err(format!("MacinRender reported filter type {other}")),
        })
    }

    const fn code(self) -> i32 {
        match self {
            Self::Peaking => 0,
            Self::LowShelf => 1,
            Self::HighShelf => 2,
            Self::LowPass => 3,
            Self::HighPass => 4,
            Self::BandPass => 5,
            Self::Notch => 6,
        }
    }
}

/// One band as the renderer reads it. A shelf's `q` is a Q, not a slope.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HptfBand {
    pub kind: HptfBandKind,
    /// A disabled band is still parsed and validated; it just never reaches the
    /// cascade. Both sides keep it so a typo in one still reports.
    pub enabled: bool,
    pub fc_hz: f64,
    pub gain_db: f64,
    pub q: f64,
}

/// A cascade the way the renderer's own parser read it, independent of any
/// sample rate and of any output existing.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HptfProfile {
    pub preamp_db: f64,
    pub bands: Vec<HptfBand>,
}

/// What the headphone feed is actually running. `applied_revision` echoes the
/// revision handed to [`Control::set_hptf`] once the audio callback has taken
/// it up, so a caller can tell a selected profile from a live one.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct HptfStatus {
    pub enabled: bool,
    pub bands: u32,
    /// The rate the sections were designed at. A biquad's response depends
    /// on it, so a caller drawing the curve has to use this one.
    pub rate: u32,
    pub preamp_db: f32,
    pub auto_trim_db: f32,
    /// Peak of the designed response over 20 Hz-20 kHz, preamp included. Above
    /// zero the compensation can clip before the output's own ceiling catches it.
    pub max_response_db: f32,
    pub applied_revision: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputKind {
    SystemSpatial,
    Stereo,
    Null,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub renderer: RendererSettings,
    pub output: OutputKind,
    pub device_id: String,
    pub input_rate: u32,
}

pub const FIELD_SCREEN_REFERENCE: u64 = 1 << 7;

#[derive(Debug, Clone, Copy, Default)]
pub struct ObjectState {
    pub active: bool,
    pub gain: f32,
    pub position: Option<[f32; 3]>,
    pub head_locked: bool,
    /// Supplied by room sessions that enable screen mapping.
    pub screen_reference: Option<bool>,
}
impl ObjectState {
    fn raw(self) -> raw::State {
        let [x, y, z] = self.position.unwrap_or([0.0; 3]);
        raw::State {
            size: size::<raw::State>(),
            valid: 3
                | 256
                | if self.position.is_some() { 4 } else { 0 }
                | if self.screen_reference.is_some() {
                    FIELD_SCREEN_REFERENCE
                } else {
                    0
                },
            active: i32::from(self.active),
            gain: self.gain,
            x,
            y,
            z,
            head_locked: i32::from(self.head_locked),
            screen: i32::from(self.screen_reference.unwrap_or_default()),
            ..Default::default()
        }
    }
}
#[derive(Debug, Clone, Copy)]
pub struct Update {
    pub element: u64,
    pub offset: u32,
    pub ramp: u32,
    /// Renderer-native mask: active=1, linear gain=2, Cartesian position=4, screen reference=128, head lock=256.
    pub changed: u64,
    pub state: ObjectState,
}
pub struct Plane<'a> {
    pub element: u64,
    pub samples: &'a [f32],
}
pub struct Frame<'a> {
    pub epoch: u64,
    pub generation: u64,
    pub start: i64,
    pub duration: u32,
    pub complete: bool,
    pub planes: &'a [Plane<'a>],
    pub initial: &'a [(u64, ObjectState)],
    pub updates: &'a [Update],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Paused,
    Playing,
    Buffering,
    Draining,
    Ended,
    Failed,
}
#[derive(Debug, Clone, Copy)]
pub struct Status {
    pub phase: Phase,
    pub epoch: u64,
    pub consumed: u64,
    pub presented: u64,
    pub queued: u64,
    pub underruns: u64,
    pub media_clock: bool,
    pub recovering: bool,
}

struct Shared {
    api: Api,
    context: *mut c_void,
    stream: *mut c_void,
    output: *mut c_void,
    initial_room_manifest: Option<String>,
    gate: Mutex<()>,
}
// SAFETY: the native Scene/output APIs support concurrent producer/control calls.
// gate covers borrowed-error calls; backend preparation owns its per-call error. All
// production methods require the unique Session; controls cannot submit PCM.
unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}
impl Drop for Shared {
    fn drop(&mut self) {
        // SAFETY: last Arc, so no caller remains. Output stops native callbacks
        // before releasing stream/context; each destroy explicitly permits NULL.
        unsafe {
            (self.api.adm_destroy_scene_output)(self.output);
            (self.api.adm_destroy_scene_stream)(self.stream);
            (self.api.adm_destroy_context)(self.context);
        }
    }
}
impl Shared {
    fn create_stream(settings: &RendererSettings, sample_rate: u32) -> Result<Self, String> {
        let api = Api::load()?;
        // SAFETY: validated no-argument constructor.
        let context = unsafe { (api.adm_create_context)() };
        if context.is_null() {
            return Err("Cannot create MacinRender context".into());
        }
        let mut owned = Shared {
            api,
            context,
            stream: std::ptr::null_mut(),
            output: std::ptr::null_mut(),
            initial_room_manifest: None,
            gate: Mutex::new(()),
        };
        let screens = match settings.room().and_then(|room| room.screens.as_deref()) {
            Some(json) => Some(string(json)?),
            None => None,
        };
        let mut stream = std::ptr::null_mut();
        let code = settings.with_raw(|rendering| {
            let raw = raw::StreamConfig {
                size: size::<raw::StreamConfig>(),
                rendering: *rendering,
                input_rate: sample_rate,
                output_rate: 48_000,
                ..Default::default()
            };
            match settings.room() {
                // SAFETY: every borrowed string/config lives until this synchronous call returns.
                None => Ok(unsafe {
                    (owned.api.adm_create_scene_stream)(context, &raw const raw, &raw mut stream)
                }),
                // SAFETY: as above; the stream retains the room package it was handed.
                Some(room) => {
                    let package = RoomPackage::load(&room.manifest)?;
                    owned.initial_room_manifest = Some(package.manifest_json());
                    package.with_config(room, |config| unsafe {
                        (owned.api.adm_create_scene_stream_with_room_quality)(
                            context,
                            &raw const raw,
                            config,
                            ROOM_QUALITY_REALTIME,
                            screens
                                .as_ref()
                                .map_or(std::ptr::null(), |json| json.as_ptr()),
                            &raw mut stream,
                        )
                    })
                }
            }
        })??;
        owned.stream = stream;
        owned.error(code, 0)?;
        Ok(owned)
    }
    fn error(&self, code: i32, domain: u8) -> Result<(), String> {
        if code == 0 {
            return Ok(());
        }
        // SAFETY: callers hold gate. The selected handle lives in this Arc, and
        // the pointer is copied before another C ABI call can invalidate it.
        let pointer = unsafe {
            match domain {
                1 => (self.api.adm_scene_stream_last_error_message)(self.stream),
                2 => (self.api.adm_scene_output_last_error_message)(self.output),
                _ => (self.api.adm_context_last_error_message)(self.context),
            }
        };
        let message = copy_text(pointer);
        Err(if message.is_empty() {
            format!("MacinRender error {code}")
        } else {
            message
        })
    }
}
fn copy_text(pointer: *const c_char) -> String {
    if pointer.is_null() {
        return String::new();
    }
    // SAFETY: internal call sites hold the appropriate native string lifetime.
    unsafe { CStr::from_ptr(pointer) }
        .to_string_lossy()
        .into_owned()
}

/// Check the incremental Scene entry point without creating an audio device.
/// A file-monitor renderer may exist while this entry point still rejects it.
pub fn probe_renderer(settings: &RendererSettings, sample_rate: u32) -> Result<(), String> {
    Shared::create_stream(settings, sample_rate).map(|_| ())
}

/// Exercise the player's object + LFE contract through the worker, using only
/// the timer-driven null output. Creating a stream alone cannot catch an
/// asynchronously rejected generation or effective initial state.
pub fn probe_player_renderer(settings: &RendererSettings, sample_rate: u32) -> Result<(), String> {
    use std::time::{Duration, Instant};
    let mut session = Session::new(&Config {
        renderer: settings.clone(),
        output: OutputKind::Null,
        device_id: String::new(),
        input_rate: sample_rate,
    })?;
    session.reset(1, 0)?;
    session.configure_lfes(1, 1, &[0], &[1])?;
    let state = ObjectState {
        active: true,
        gain: 1.0,
        position: None,
        head_locked: false,
        screen_reference: None,
    };
    if !session.submit(&Frame {
        epoch: 1,
        generation: 1,
        start: 0,
        duration: 64,
        complete: true,
        planes: &[
            Plane {
                element: 0,
                samples: &[],
            },
            Plane {
                element: 1,
                samples: &[],
            },
        ],
        initial: &[
            (
                0,
                ObjectState {
                    position: Some([0.0, 1.0, 0.0]),
                    ..state
                },
            ),
            (1, state),
        ],
        updates: &[],
    })? {
        return Err("Renderer capability probe could not submit its Scene".into());
    }
    session.end(1, 64)?;
    let control = session.control();
    control.play(true)?;
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let status = control.status()?;
        if status.phase == Phase::Failed {
            return Err(control.failure_message());
        }
        // A room's reverberation plays on past the programme's end.
        if status.phase == Phase::Ended
            && (status.presented == 64 || (settings.room().is_some() && status.presented > 64))
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!("Renderer capability probe timed out: {status:?}"));
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

pub struct Session {
    inner: Arc<Shared>,
    speaker_renderer: SpeakerRenderer,
    silent_bed: Vec<u64>,
}

#[cfg(all(test, native_macinrender))]
mod lfe_tests;
#[derive(Clone)]
pub struct Control {
    inner: Arc<Shared>,
}

impl Session {
    pub fn new(config: &Config) -> Result<Self, String> {
        let mut owned = Shared::create_stream(&config.renderer, config.input_rate)?;
        let layout = string(&config.renderer.layout)?;
        let device = string(&config.device_id)?;
        let raw = raw::OutputConfig {
            size: size::<raw::OutputConfig>(),
            kind: match config.output {
                OutputKind::SystemSpatial => 0,
                OutputKind::Stereo => 1,
                OutputKind::Null => 2,
            },
            layout: layout.as_ptr(),
            device: device.as_ptr(),
            geometry: config.renderer.geometry(),
            reserved: 0,
        };
        // SAFETY: stream/config are live; ownership of the result passes to Shared.
        let code = unsafe {
            (owned.api.adm_create_scene_output)(
                owned.context,
                owned.stream,
                &raw const raw,
                &raw mut owned.output,
            )
        };
        owned.error(code, 0)?;
        Ok(Self {
            inner: Arc::new(owned),
            speaker_renderer: if config.renderer.binaural {
                SpeakerRenderer::SafVbap
            } else {
                config.renderer.speaker_renderer
            },
            silent_bed: Vec::new(),
        })
    }
    #[must_use]
    pub fn control(&self) -> Control {
        Control {
            inner: Arc::clone(&self.inner),
        }
    }
    /// The room geometry used by head-locked sources for this session.
    #[must_use]
    pub fn initial_room_manifest(&self) -> Option<&str> {
        self.inner.initial_room_manifest.as_deref()
    }
    pub fn reset(&mut self, epoch: u64, target: i64) -> Result<(), String> {
        let s = &self.inner;
        let _guard = s.gate.lock().unwrap();
        // SAFETY: output owns the device reset barrier; this is the sole producer.
        s.error(
            unsafe { (s.api.adm_scene_output_begin_epoch)(s.output, epoch, target) },
            2,
        )
    }
    pub fn configure(
        &mut self,
        epoch: u64,
        generation: u64,
        objects: &[u64],
        lfe: Option<u64>,
    ) -> Result<(), String> {
        self.configure_lfes(epoch, generation, objects, lfe.as_slice())
    }

    /// LFE IDs are in semantic destination order (LFE1, LFE2).
    pub fn configure_lfes(
        &mut self,
        epoch: u64,
        generation: u64,
        objects: &[u64],
        lfes: &[u64],
    ) -> Result<(), String> {
        if lfes.len() > 2 {
            return Err("The renderer supports at most two LFE destinations".into());
        }
        let pad_bed = self.speaker_renderer == SpeakerRenderer::TripleBalance && !lfes.is_empty();
        if pad_bed && lfes.len() != 1 {
            return Err(
                "Triple Balance accepts one LFE bus; fold multiple inputs before submission".into(),
            );
        }
        let mut elements: Vec<_> = objects
            .iter()
            .map(|&id| raw::Element {
                size: size::<raw::Element>(),
                id,
                ..Default::default()
            })
            .collect();
        for (&id, label) in lfes.iter().zip([c"LFE1", c"LFE2"]) {
            elements.push(raw::Element {
                size: size::<raw::Element>(),
                id,
                role: 2,
                label: label.as_ptr(),
                ..Default::default()
            });
        }
        let mut silent_bed = Vec::new();
        if pad_bed {
            // The Core's fixed-bed route requires a complete 7.1.2 descriptor
            // group. Only its LFE carries input; these other lanes stay silent.
            let mut id = u64::MAX;
            for label in [
                c"M+030", c"M-030", c"M+000", c"M+090", c"M-090", c"M+135", c"M-135", c"U+090",
                c"U-090",
            ] {
                while elements.iter().any(|element| element.id == id) {
                    id = id.checked_sub(1).ok_or("No free Scene ID for silent bed")?;
                }
                silent_bed.push(id);
                elements.push(raw::Element {
                    size: size::<raw::Element>(),
                    id,
                    role: 1,
                    label: label.as_ptr(),
                    ..Default::default()
                });
            }
        }
        let count = u32::try_from(elements.len()).map_err(|_| "Too many Scene elements")?;
        let s = &self.inner;
        let _guard = s.gate.lock().unwrap();
        // SAFETY: this descriptor array is deeply copied before the call returns.
        s.error(
            unsafe {
                (s.api.adm_scene_stream_configure_generation)(
                    s.stream,
                    epoch,
                    generation,
                    elements.as_ptr(),
                    count,
                )
            },
            1,
        )?;
        self.silent_bed = silent_bed;
        Ok(())
    }
    /// Returns false for backpressure. The caller retains and retries that frame.
    pub fn submit(&mut self, frame: &Frame<'_>) -> Result<bool, String> {
        let mut planes: Vec<_> = frame
            .planes
            .iter()
            .map(|plane| {
                Ok(raw::Plane {
                    size: size::<raw::Plane>(),
                    id: plane.element,
                    samples: plane.samples.as_ptr(),
                    count: if plane.samples.is_empty() {
                        frame.duration
                    } else {
                        u32::try_from(plane.samples.len()).map_err(|_| "PCM plane too long")?
                    },
                    stride: 1,
                    has_signal: i32::from(!plane.samples.is_empty()),
                    ..Default::default()
                })
            })
            .collect::<Result<_, &str>>()?;
        let mut initial: Vec<_> = frame
            .initial
            .iter()
            .map(|(id, state)| raw::Initial {
                size: size::<raw::Initial>(),
                id: *id,
                state: state.raw(),
                reserved: 0,
            })
            .collect();
        for &id in &self.silent_bed {
            planes.push(raw::Plane {
                size: size::<raw::Plane>(),
                id,
                count: frame.duration,
                stride: 1,
                ..Default::default()
            });
            initial.push(raw::Initial {
                size: size::<raw::Initial>(),
                id,
                state: ObjectState {
                    active: false,
                    gain: 0.0,
                    position: None,
                    head_locked: false,
                    screen_reference: None,
                }
                .raw(),
                ..Default::default()
            });
        }
        let updates: Vec<_> = frame
            .updates
            .iter()
            .map(|u| raw::Update {
                size: size::<raw::Update>(),
                id: u.element,
                offset: u.offset,
                ramp: u.ramp,
                jump: i32::from(u.ramp == 0 && u.changed & 4 != 0),
                changed: u.changed,
                state: u.state.raw(),
                ..Default::default()
            })
            .collect();
        let raw = raw::Frame {
            size: size::<raw::Frame>(),
            flags: u32::from(frame.complete),
            epoch: frame.epoch,
            generation: frame.generation,
            start: frame.start,
            duration: frame.duration,
            plane_count: u32::try_from(planes.len()).map_err(|_| "Too many planes")?,
            planes: planes.as_ptr(),
            initial_count: u32::try_from(initial.len()).map_err(|_| "Too many initial states")?,
            initial: initial.as_ptr(),
            update_count: u32::try_from(updates.len()).map_err(|_| "Too many metadata events")?,
            updates: updates.as_ptr(),
        };
        let s = &self.inner;
        let _guard = s.gate.lock().unwrap();
        let mut accepted = 3;
        // SAFETY: every pointed-to array remains alive until submit deep-copies it.
        s.error(
            unsafe {
                (s.api.adm_scene_stream_submit_frame)(
                    s.stream,
                    &raw const raw,
                    0,
                    &raw mut accepted,
                )
            },
            1,
        )?;
        match accepted {
            0 => Ok(true),
            1 | 2 => Ok(false),
            _ => Err("MacinRender Scene input closed".into()),
        }
    }
    pub fn end(&mut self, epoch: u64, end: i64) -> Result<(), String> {
        let s = &self.inner;
        let _guard = s.gate.lock().unwrap();
        // SAFETY: called only by the producer after its last accepted frame.
        s.error(
            unsafe { (s.api.adm_scene_stream_signal_end)(s.stream, epoch, end) },
            1,
        )
    }
}

impl Control {
    pub fn play(&self, playing: bool) -> Result<(), String> {
        let s = &self.inner;
        let _guard = s.gate.lock().unwrap();
        // SAFETY: the Arc owns the output; native controls serialize with consumption.
        s.error(
            unsafe {
                if playing {
                    (s.api.adm_scene_output_play)(s.output)
                } else {
                    (s.api.adm_scene_output_pause)(s.output)
                }
            },
            2,
        )
    }
    pub fn volume(&self, gain: f32) -> Result<(), String> {
        let s = &self.inner;
        let _guard = s.gate.lock().unwrap();
        // SAFETY: native code validates the scalar and serializes the control.
        s.error(
            unsafe { (s.api.adm_scene_output_set_volume)(s.output, gain) },
            2,
        )
    }
    pub fn orientation(&self, pose: [f32; 3]) -> Result<(), String> {
        let s = &self.inner;
        let _guard = s.gate.lock().unwrap();
        // SAFETY: pose is copied synchronously, with native finite-value validation.
        s.error(
            unsafe {
                (s.api.adm_scene_stream_set_listener_orientation)(
                    s.stream, pose[0], pose[1], pose[2],
                )
            },
            1,
        )
    }
    pub fn switch_renderer(&self, settings: &RendererSettings) -> Result<(), String> {
        let s = &self.inner;
        let mut error = std::ptr::null_mut();
        let code = settings.with_raw(|raw| {
            // SAFETY: the Arc retains the stream. This entrypoint owns its error
            // string, and native preparation runs concurrently with the producer.
            unsafe { (s.api.adm_scene_stream_switch_backend_ex)(s.stream, raw, &raw mut error) }
        })?;
        let message = copy_text(error);
        // SAFETY: ownership was transferred by the _ex entrypoint.
        unsafe {
            (s.api.adm_free_string)(error);
        }
        if code == 0 {
            Ok(())
        } else {
            Err(if message.is_empty() {
                format!("MacinRender backend error {code}")
            } else {
                message
            })
        }
    }
    /// Queue an already loaded package. The returned revision is kept pending
    /// until the room status reports that it applied or failed.
    pub fn request_room(&self, room: &RoomSettings, package: &RoomPackage) -> Result<u64, String> {
        let s = &self.inner;
        let mut request = 0;
        let mut error = std::ptr::null_mut();
        let code = package.with_config(room, |config| {
            // SAFETY: the Arc retains the stream. The request retains the
            // package it needs and transfers ownership of any error string.
            unsafe {
                (s.api.adm_scene_stream_switch_room)(
                    s.stream,
                    config,
                    &raw mut request,
                    &raw mut error,
                )
            }
        })?;
        let message = copy_text(error);
        // SAFETY: ownership was transferred by the switch entry point.
        unsafe { (s.api.adm_free_string)(error) };
        if code == 0 {
            Ok(request)
        } else if message.is_empty() {
            Err(format!("MacinRender room error {code}"))
        } else {
            Err(message)
        }
    }
    /// The room renderer's own account of its room; `None` for a session that
    /// renders none.
    pub fn try_room_status(&self) -> Result<Option<RoomStatus>, String> {
        let s = &self.inner;
        let mut json = std::ptr::null_mut();
        // SAFETY: the Arc retains the stream; the status string is owned by the caller.
        let code = unsafe { (s.api.adm_scene_stream_room_status_json)(s.stream, &raw mut json) };
        let text = copy_text(json);
        // SAFETY: ownership was transferred; NULL is permitted.
        unsafe { (s.api.adm_free_string)(json) };
        match code {
            0 => RoomStatus::parse(&text).map(Some),
            UNSUPPORTED => Ok(None),
            _ => Err(format!("MacinRender room status error {code}")),
        }
    }

    /// Where the room renderer last placed the Scene element `element`; see
    /// [`RoomSource`]. `None` outside a room session too.
    pub fn room_source(&self, element: u64) -> Result<Option<RoomSource>, String> {
        let s = &self.inner;
        let mut json = std::ptr::null_mut();
        // SAFETY: the Arc retains the stream; the source string is owned by the caller.
        let code =
            unsafe { (s.api.adm_scene_stream_room_source_json)(s.stream, element, &raw mut json) };
        let text = copy_text(json);
        // SAFETY: ownership was transferred; NULL is permitted.
        unsafe { (s.api.adm_free_string)(json) };
        match code {
            0 => RoomSource::parse(&text),
            UNSUPPORTED => Ok(None),
            _ => Err(format!("MacinRender room source error {code}")),
        }
    }
    /// Load, replace or (with an empty profile) disable headphone compensation.
    ///
    /// `Ok(false)` reports that this output has no two-channel headphone feed to
    /// compensate — the multichannel and system-spatial paths hand the operating
    /// system a bed and never form the final stereo signal on our side. The
    /// renderer answers that case explicitly instead of ignoring the request, so
    /// it is a state to display, not a failure. Parsing and coefficient design
    /// run synchronously here, so a bad file fails on this thread and never
    /// reaches the audio callback.
    pub fn set_hptf(&self, settings: &HptfSettings, revision: u64) -> Result<bool, String> {
        let s = &self.inner;
        let profile = string(&settings.profile)?;
        let raw = raw::HptfConfig {
            size: size::<raw::HptfConfig>(),
            profile: profile.as_ptr(),
            preamp_mode: i32::from(settings.auto_trim),
            reserved: 0,
            revision,
        };
        let mut error = std::ptr::null_mut();
        // SAFETY: the Arc retains the output and the config lives through this
        // call. Preparation supports concurrent producer/control calls and owns
        // its error string, so neither native nor Rust's gate spans file I/O.
        let code = unsafe {
            (s.api.adm_scene_output_set_hptf_ex)(s.output, &raw const raw, &raw mut error)
        };
        let message = copy_text(error);
        // SAFETY: the _ex entrypoint transfers ownership, including on failure.
        unsafe { (s.api.adm_free_string)(error) };
        match code {
            0 => Ok(true),
            UNSUPPORTED => Ok(false),
            _ => Err(if message.is_empty() {
                format!("MacinRender headphone profile error {code}")
            } else {
                message
            }),
        }
    }
    /// Apply a cascade held in memory, with no file for it to go through.
    ///
    /// Same acceptance and same crossfade as [`Control::set_hptf`], and the same
    /// `Ok(false)` for an output with no headphone feed. `revision` is echoed by
    /// `applied_revision` once the callback has taken it up. Design is
    /// synchronous on this thread; never call it from an audio callback.
    pub fn set_hptf_parameters(
        &self,
        profile: &HptfProfile,
        auto_trim: bool,
        revision: u64,
    ) -> Result<bool, String> {
        let s = &self.inner;
        // Every element carries the stride, because the renderer walks an array
        // it did not allocate and a newer caller may append fields.
        let bands: Vec<raw::HptfBand> = profile
            .bands
            .iter()
            .map(|band| raw::HptfBand {
                size: size::<raw::HptfBand>(),
                kind: band.kind.code(),
                enabled: i32::from(band.enabled),
                reserved: 0,
                fc_hz: band.fc_hz,
                gain_db: band.gain_db,
                q: band.q,
            })
            .collect();
        let raw = raw::HptfParameters {
            size: size::<raw::HptfParameters>(),
            band_count: u32::try_from(bands.len())
                .map_err(|_| "Too many filter bands to submit".to_owned())?,
            bands: bands.as_ptr(),
            preamp_db: profile.preamp_db,
            preamp_mode: i32::from(auto_trim),
            reserved: 0,
            revision,
        };
        let _guard = s.gate.lock().unwrap();
        // SAFETY: the Arc retains the output, and the snapshot plus its band
        // array live until this synchronous call returns; nothing is retained.
        let code =
            unsafe { (s.api.adm_scene_output_set_hptf_parameters)(s.output, &raw const raw) };
        if code == UNSUPPORTED {
            return Ok(false);
        }
        s.error(code, 2).map(|()| true)
    }
    /// The compensation the output is running now, or the disabled default when
    /// this output cannot carry one.
    pub fn hptf_status(&self) -> Result<HptfStatus, String> {
        let s = &self.inner;
        let _guard = s.gate.lock().unwrap();
        let mut info = raw::HptfInfo {
            size: size::<raw::HptfInfo>(),
            ..Default::default()
        };
        // SAFETY: the Arc retains the output, and the struct is written whole
        // before this synchronous call returns.
        let code = unsafe { (s.api.adm_scene_output_get_hptf_info)(s.output, &raw mut info) };
        if code == UNSUPPORTED {
            return Ok(HptfStatus::default());
        }
        s.error(code, 2)?;
        Ok(HptfStatus {
            enabled: info.enabled != 0,
            bands: info.band_count,
            rate: info.sample_rate,
            preamp_db: info.preamp_db,
            auto_trim_db: info.auto_trim_db,
            max_response_db: info.max_response_db,
            applied_revision: info.applied_revision,
        })
    }
    pub fn status(&self) -> Result<Status, String> {
        let s = &self.inner;
        let _guard = s.gate.lock().unwrap();
        let mut raw = raw::OutputStatus {
            size: size::<raw::OutputStatus>(),
            ..Default::default()
        };
        // SAFETY: the status struct is initialized with its verified ABI size.
        s.error(
            unsafe { (s.api.adm_scene_output_get_status)(s.output, &raw mut raw) },
            2,
        )?;
        Ok(Status {
            phase: match raw.state {
                0 => Phase::Paused,
                1 => Phase::Playing,
                2 => Phase::Buffering,
                3 => Phase::Draining,
                4 => Phase::Ended,
                _ => Phase::Failed,
            },
            epoch: raw.epoch,
            consumed: raw.consumed,
            presented: raw.presented,
            queued: raw.queued,
            underruns: raw.underruns,
            media_clock: raw.clock == 1,
            recovering: raw.recovering != 0,
        })
    }

    #[must_use]
    pub fn failure_message(&self) -> String {
        let s = &self.inner;
        let _guard = s.gate.lock().unwrap();
        // SAFETY: the handles are retained and all borrowed messages are copied
        // while the gate excludes another non-pull call from this binding.
        let count = unsafe { (s.api.adm_scene_stream_log_count)(s.stream) };
        for index in (count.saturating_sub(32)..count).rev() {
            let mut diagnostic = raw::Diagnostic {
                size: size::<raw::Diagnostic>(),
                ..Default::default()
            };
            // SAFETY: the initialized POD has the C header's verified layout.
            if unsafe { (s.api.adm_scene_stream_log_entry)(s.stream, index, &raw mut diagnostic) }
                != 0
                && diagnostic.code == 6
            {
                return copy_text(diagnostic.message);
            }
        }
        // SAFETY: output is retained; copy precedes the next control call.
        let message = copy_text(unsafe { (s.api.adm_scene_output_last_error_message)(s.output) });
        if message.is_empty() {
            "MacinRender rendering or output device failed".into()
        } else {
            message
        }
    }
}

/// What the renderer's own parser makes of `ParametricEQ` text.
///
/// No file is opened and no output has to exist, which is what lets a caller
/// check its own reading of a profile before committing anyone to it. Passing
/// the same text to both parsers, rather than the same path, also removes "the
/// file changed in between" as an explanation for a disagreement.
pub fn parse_parametric_eq(text: &str) -> Result<HptfProfile, String> {
    let api = Api::load()?;
    let text = CString::new(text).map_err(|_| "A profile cannot contain a NUL byte".to_owned())?;
    // SAFETY: constructor has no arguments; the temporary context is released below.
    let context = unsafe { (api.adm_create_context)() };
    if context.is_null() {
        return Err("Cannot create a profile parsing context".into());
    }
    let parsed = parse_with(&api, context, &text);
    // SAFETY: this context belongs to this function and nothing borrows it now.
    unsafe { (api.adm_destroy_context)(context) };
    parsed
}

/// The body of [`parse_parametric_eq`], split out so every exit releases the
/// context.
fn parse_with(api: &Api, context: *mut c_void, text: &CStr) -> Result<HptfProfile, String> {
    let failure = || {
        // SAFETY: the message is borrowed from the context, which outlives this
        // copy of it.
        let message = copy_text(unsafe { (api.adm_context_last_error_message)(context) });
        if message.is_empty() {
            "MacinRender cannot read this profile".to_owned()
        } else {
            message
        }
    };
    let mut preamp_db = 0.0;
    let mut count = 0_u32;
    // A null array with zero capacity asks how many bands there are, disabled
    // ones included, rather than writing any.
    // SAFETY: the text and both out pointers are valid for this call.
    let code = unsafe {
        (api.adm_hptf_parse_parametric_eq)(
            context,
            text.as_ptr(),
            &raw mut preamp_db,
            std::ptr::null_mut(),
            0,
            &raw mut count,
        )
    };
    if code != 0 {
        return Err(failure());
    }
    let mut bands = vec![
        raw::HptfBand {
            size: size::<raw::HptfBand>(),
            ..Default::default()
        };
        count as usize
    ];
    let mut written = 0_u32;
    if count > 0 {
        // SAFETY: the array is ours, `count` elements long, and each element
        // carries the stride the renderer writes at.
        let code = unsafe {
            (api.adm_hptf_parse_parametric_eq)(
                context,
                text.as_ptr(),
                &raw mut preamp_db,
                bands.as_mut_ptr(),
                count,
                &raw mut written,
            )
        };
        if code != 0 {
            return Err(failure());
        }
        bands.truncate(written as usize);
    }
    Ok(HptfProfile {
        preamp_db,
        bands: bands
            .into_iter()
            .map(|band| {
                Ok(HptfBand {
                    kind: HptfBandKind::of(band.kind)?,
                    enabled: band.enabled != 0,
                    fc_hz: band.fc_hz,
                    gain_db: band.gain_db,
                    q: band.q,
                })
            })
            .collect::<Result<_, String>>()?,
    })
}

pub fn output_devices() -> Result<Vec<(String, String, bool)>, String> {
    let api = Api::load()?;
    // SAFETY: constructor has no arguments; the temporary context is released below.
    let context = unsafe { (api.adm_create_context)() };
    if context.is_null() {
        return Err("Cannot create device query context".into());
    }
    let mut json = std::ptr::null_mut();
    // SAFETY: out pointer is valid; JSON ownership is transferred to this caller.
    let code = unsafe { (api.adm_monitor_output_devices_json)(context, &raw mut json) };
    let text = copy_text(json);
    // SAFETY: these handles belong to this function, including a nullable string.
    unsafe {
        (api.adm_free_string)(json);
        (api.adm_destroy_context)(context);
    }
    if code != 0 {
        return Err("Cannot enumerate playback devices".into());
    }
    let data: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    Ok(data
        .as_array()
        .ok_or("Invalid device list")?
        .iter()
        .filter_map(|value| {
            Some((
                value["id"].as_str()?.into(),
                value["name"].as_str()?.into(),
                value["default"].as_bool().unwrap_or(false),
            ))
        })
        .collect())
}

#[cfg(all(test, native_macinrender))]
mod tests {
    use super::*;
    #[test]
    fn scene_capability_probe_opens_no_output_and_checks_the_requested_layout() {
        let mut renderer = RendererSettings {
            binaural: false,
            speaker_renderer: SpeakerRenderer::SafVbap,
            layout: "4+7+0".into(),
            sofa: String::new(),
            split_lfe: false,
            room: None,
        };
        let stream = Shared::create_stream(&renderer, 48_000).unwrap();
        assert!(stream.output.is_null());
        assert!(!stream.stream.is_null());
        drop(stream);
        renderer.layout = "invalid-layout".into();
        assert!(probe_renderer(&renderer, 48_000).is_err());
        renderer.layout = "4+7+0".into();
        assert!(
            probe_renderer(&renderer, 48_000).is_ok(),
            "a failed probe must not poison the next stream"
        );
    }
    unsafe extern "C" {
        fn macinrender_abi_size(index: u32) -> usize;
        fn macinrender_abi_offset(index: u32) -> usize;
    }
    #[test]
    fn rust_layouts_match_the_actual_c_header() {
        let sizes = [
            size_of::<raw::RendererConfig>(),
            size_of::<raw::StreamConfig>(),
            size_of::<raw::Element>(),
            size_of::<raw::State>(),
            size_of::<raw::Plane>(),
            size_of::<raw::Initial>(),
            size_of::<raw::Update>(),
            size_of::<raw::Frame>(),
            size_of::<raw::OutputConfig>(),
            size_of::<raw::OutputStatus>(),
            size_of::<raw::HeadSample>(),
            size_of::<raw::HptfConfig>(),
            size_of::<raw::HptfInfo>(),
            size_of::<raw::HptfBand>(),
            size_of::<raw::HptfParameters>(),
            size_of::<raw::Diagnostic>(),
        ];
        for (index, size) in sizes.into_iter().enumerate() {
            // SAFETY: probe is built from the upstream header and returns a scalar.
            assert_eq!(
                size,
                unsafe { macinrender_abi_size(u32::try_from(index).unwrap()) },
                "POD {index}"
            );
        }
        let offsets = [
            std::mem::offset_of!(raw::RendererConfig, geometry),
            std::mem::offset_of!(raw::StreamConfig, input_rate),
            std::mem::offset_of!(raw::StreamConfig, input_bytes),
            std::mem::offset_of!(raw::Element, identity),
            std::mem::offset_of!(raw::State, valid),
            std::mem::offset_of!(raw::State, gain),
            std::mem::offset_of!(raw::State, head_locked),
            std::mem::offset_of!(raw::Plane, samples),
            std::mem::offset_of!(raw::Plane, has_signal),
            std::mem::offset_of!(raw::Initial, state),
            std::mem::offset_of!(raw::Update, changed),
            std::mem::offset_of!(raw::Update, state),
            std::mem::offset_of!(raw::Frame, start),
            std::mem::offset_of!(raw::Frame, planes),
            std::mem::offset_of!(raw::Frame, updates),
            std::mem::offset_of!(raw::OutputConfig, geometry),
            std::mem::offset_of!(raw::OutputStatus, presented),
            std::mem::offset_of!(raw::OutputStatus, clock),
            std::mem::offset_of!(raw::HeadSample, w),
            std::mem::offset_of!(raw::HptfConfig, profile),
            std::mem::offset_of!(raw::HptfConfig, revision),
            std::mem::offset_of!(raw::HptfInfo, applied_revision),
            std::mem::offset_of!(raw::HptfBand, fc_hz),
            std::mem::offset_of!(raw::HptfBand, q),
            std::mem::offset_of!(raw::HptfParameters, bands),
            std::mem::offset_of!(raw::HptfParameters, revision),
            std::mem::offset_of!(raw::Diagnostic, message),
            std::mem::offset_of!(raw::Diagnostic, code),
        ];
        for (index, offset) in offsets.into_iter().enumerate() {
            // SAFETY: C's offsetof probe returns a scalar from the pinned headers.
            assert_eq!(
                offset,
                unsafe { macinrender_abi_offset(u32::try_from(index).unwrap()) },
                "field {index}"
            );
        }
    }

    fn feed_test_tone(
        mut session: Session,
        stop: &std::sync::atomic::AtomicBool,
        elements: &[u64],
    ) {
        use std::sync::atomic::Ordering;
        let samples = [0.01; 480];
        let initial: Vec<_> = elements
            .iter()
            .map(|&id| {
                (
                    id,
                    ObjectState {
                        active: true,
                        gain: 1.0,
                        position: Some([0.0, 1.0, 0.0]),
                        head_locked: false,
                        screen_reference: None,
                    },
                )
            })
            .collect();
        let planes: Vec<_> = elements
            .iter()
            .map(|&id| Plane {
                element: id,
                samples: &samples,
            })
            .collect();
        let mut start = 0;
        while !stop.load(Ordering::Relaxed) {
            if session
                .submit(&Frame {
                    epoch: 1,
                    generation: 1,
                    start,
                    duration: 480,
                    complete: true,
                    planes: &planes,
                    initial: &initial,
                    updates: &[],
                })
                .unwrap()
            {
                start += 480;
            } else {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
    }

    fn check_takeover_continuity(control: &Control, underruns: u64) {
        use std::time::{Duration, Instant};
        let until = Instant::now() + Duration::from_millis(500);
        let mut frame = control.status().unwrap().presented;
        let mut progressed = Instant::now();
        while Instant::now() < until {
            let status = control.status().unwrap();
            assert_eq!(
                status.underruns, underruns,
                "renderer takeover interrupted audio"
            );
            if status.presented > frame {
                frame = status.presented;
                progressed = Instant::now();
            }
            assert!(
                progressed.elapsed() < Duration::from_millis(250),
                "renderer takeover stalled"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn repeated_epochs_start_with_continuous_head_tracking() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::{Duration, Instant};
        let mut session = Session::new(&Config {
            renderer: RendererSettings {
                speaker_renderer: SpeakerRenderer::SafVbap,
                binaural: true,
                layout: "4+7+0".into(),
                sofa: String::new(),
                split_lfe: true,
                room: None,
            },
            output: OutputKind::Null,
            device_id: String::new(),
            input_rate: 48_000,
        })
        .unwrap();
        let control = session.control();
        let pose_control = control.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let finish = Arc::clone(&stop);
        let poses = std::thread::spawn(move || {
            let mut yaw = 0.0;
            while !finish.load(Ordering::Relaxed) {
                pose_control.orientation([yaw, 0.0, 0.0]).unwrap();
                yaw = (yaw + 0.1) % 30.0;
                std::thread::sleep(Duration::from_millis(2));
            }
        });
        let samples = [0.01; 2048];
        let initial = [(
            7,
            ObjectState {
                active: true,
                gain: 1.0,
                position: Some([0.0, 1.0, 0.0]),
                head_locked: false,
                screen_reference: None,
            },
        )];
        let planes = [Plane {
            element: 7,
            samples: &samples,
        }];
        let mut longest_start = Duration::ZERO;
        for epoch in 1..=32 {
            session.reset(epoch, 0).unwrap();
            session.configure(epoch, 1, &[7], None).unwrap();
            control.play(true).unwrap();
            let started = Instant::now();
            assert!(
                session
                    .submit(&Frame {
                        epoch,
                        generation: 1,
                        start: 0,
                        duration: 2048,
                        complete: true,
                        planes: &planes,
                        initial: &initial,
                        updates: &[],
                    })
                    .unwrap()
            );
            while control.status().unwrap().presented == 0 {
                assert!(
                    started.elapsed() < Duration::from_millis(500),
                    "replay {epoch} remained at frame zero"
                );
                std::thread::sleep(Duration::from_millis(2));
            }
            longest_start = longest_start.max(started.elapsed());
            session.end(epoch, 2048).unwrap();
            while control.status().unwrap().phase != Phase::Ended {
                assert!(
                    started.elapsed() < Duration::from_secs(1),
                    "replay {epoch} did not drain"
                );
                std::thread::sleep(Duration::from_millis(2));
            }
            assert_eq!(control.status().unwrap().presented, 2048);
        }
        stop.store(true, Ordering::Relaxed);
        poses.join().unwrap();
        eprintln!("32 tracked replays; longest start {longest_start:?}");
    }

    #[test]
    #[ignore = "requires MACINDECODE_AC4_TEST_SOFA; validates concurrent HRTF preparation"]
    fn hrtf_preparation_keeps_the_producer_and_output_controls_live() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::{Duration, Instant};
        let path = std::env::var("MACINDECODE_AC4_TEST_SOFA").expect("set SOFA path");
        let settings = RendererSettings {
            speaker_renderer: SpeakerRenderer::SafVbap,
            binaural: true,
            layout: "4+7+0".into(),
            sofa: String::new(),
            split_lfe: true,
            room: None,
        };
        let mut session = Session::new(&Config {
            renderer: settings.clone(),
            output: OutputKind::Null,
            device_id: String::new(),
            input_rate: 48_000,
        })
        .unwrap();
        session.reset(1, 0).unwrap();
        let elements: Vec<u64> = (7..27).collect();
        session.configure(1, 1, &elements, None).unwrap();
        let control = session.control();
        control.play(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let producer = std::thread::spawn(move || feed_test_tone(session, &worker_stop, &elements));
        let deadline = Instant::now() + Duration::from_secs(40);
        while control.status().unwrap().presented < 4800 {
            assert!(Instant::now() < deadline, "startup timed out");
            std::thread::sleep(Duration::from_millis(10));
        }
        let invalid = RendererSettings {
            sofa: "/missing/listener.sofa".into(),
            ..settings.clone()
        };
        assert!(control.switch_renderer(&invalid).is_err());
        let before = control.status().unwrap().presented;
        let underruns = control.status().unwrap().underruns;
        let cached_settings = settings.clone();
        let loader_control = control.clone();
        // Large user datasets take substantially longer with OpenBLAS/MSVC
        // than Accelerate. Bound preparation separately from realtime calls.
        let preparation_start = Instant::now();
        let preparation_deadline = preparation_start + Duration::from_secs(120);
        let loader = std::thread::spawn(move || {
            loader_control.switch_renderer(&RendererSettings {
                sofa: path,
                ..settings
            })
        });
        let mut advanced = false;
        let mut last_frame = before;
        let mut last_progress = Instant::now();
        let mut longest_gap = Duration::ZERO;
        while !loader.is_finished() {
            let call_start = Instant::now();
            let status = control.status().unwrap();
            assert!(
                call_start.elapsed() < Duration::from_secs(1),
                "status blocked behind HRTF preparation"
            );
            assert_ne!(status.phase, Phase::Failed);
            assert_eq!(
                status.underruns, underruns,
                "HRTF preparation interrupted audio"
            );
            if status.presented > last_frame {
                longest_gap = longest_gap.max(last_progress.elapsed());
                last_progress = Instant::now();
                last_frame = status.presented;
            }
            assert!(
                last_progress.elapsed() < Duration::from_millis(250),
                "media stalled while preparing HRTFs"
            );
            advanced |= status.presented > before;
            assert!(
                Instant::now() < preparation_deadline,
                "HRTF preparation timed out"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let outcome = loader.join().unwrap();
        assert!(outcome.is_ok(), "{outcome:?}");
        assert!(
            advanced,
            "media did not advance while preparing the SOFA dataset"
        );
        eprintln!(
            "SOFA prepared in {:?}; longest presentation gap {longest_gap:?}",
            preparation_start.elapsed()
        );
        check_takeover_continuity(&control, underruns);
        let cached = Instant::now();
        control.switch_renderer(&cached_settings).unwrap();
        assert!(
            cached.elapsed() < Duration::from_millis(500),
            "KEMAR interpolation tables were rebuilt"
        );
        check_takeover_continuity(&control, underruns);
        stop.store(true, Ordering::Relaxed);
        producer.join().unwrap();
    }
}
