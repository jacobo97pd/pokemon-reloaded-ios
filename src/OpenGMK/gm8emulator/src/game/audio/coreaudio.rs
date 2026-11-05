//! Native iOS audio output through a RemoteIO audio unit.
//!
//! The counterpart of `aaudio.rs`, with the same functions, so `audio.rs` drives both phones the
//! same way: the engine feeds a `Mixer`, and the unit's render callback pulls straight from it.
//! The unit converts the mixer's rate to the hardware's, so the mixer can keep a fixed 48 kHz.
//!
//! AudioToolbox is a plain C API, so this is plain FFI. The audio session (category, interruptions
//! by calls or alarms) is Objective-C and lives in the app (`ios/host/main.m`), which reports
//! interruptions through `set_focus_paused`.

use std::{
    ffi::c_void,
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering},
        Arc, Mutex,
    },
};

use udon::source::{ChannelCount, Sample, SampleRate, Source};

use super::{
    mixer::{Mixer, MixerHandle},
    GbAudioParams,
};

type OSStatus = i32;
type AudioUnit = *mut c_void;
type AudioComponent = *mut c_void;

#[repr(C)]
struct AudioComponentDescription {
    component_type: u32,
    component_sub_type: u32,
    component_manufacturer: u32,
    component_flags: u32,
    component_flags_mask: u32,
}

#[repr(C)]
struct AudioStreamBasicDescription {
    sample_rate: f64,
    format_id: u32,
    format_flags: u32,
    bytes_per_packet: u32,
    frames_per_packet: u32,
    bytes_per_frame: u32,
    channels_per_frame: u32,
    bits_per_channel: u32,
    reserved: u32,
}

#[repr(C)]
struct AudioBuffer {
    number_channels: u32,
    data_byte_size: u32,
    data: *mut c_void,
}

#[repr(C)]
struct AudioBufferList {
    number_buffers: u32,
    buffers: [AudioBuffer; 1],
}

type RenderCallback = extern "C" fn(
    ref_con: *mut c_void,
    action_flags: *mut u32,
    time_stamp: *const c_void,
    bus_number: u32,
    number_frames: u32,
    data: *mut AudioBufferList,
) -> OSStatus;

#[repr(C)]
struct AURenderCallbackStruct {
    input_proc: RenderCallback,
    input_proc_ref_con: *mut c_void,
}

const fn four_cc(code: &[u8; 4]) -> u32 {
    (code[0] as u32) << 24 | (code[1] as u32) << 16 | (code[2] as u32) << 8 | code[3] as u32
}

const K_AUDIO_UNIT_TYPE_OUTPUT: u32 = four_cc(b"auou");
const K_AUDIO_UNIT_SUB_TYPE_REMOTE_IO: u32 = four_cc(b"rioc");
const K_AUDIO_UNIT_MANUFACTURER_APPLE: u32 = four_cc(b"appl");
const K_AUDIO_FORMAT_LINEAR_PCM: u32 = four_cc(b"lpcm");
const K_AUDIO_FORMAT_FLAG_IS_FLOAT: u32 = 1 << 0;
const K_AUDIO_FORMAT_FLAG_IS_PACKED: u32 = 1 << 3;
const K_AUDIO_UNIT_PROPERTY_STREAM_FORMAT: u32 = 8;
const K_AUDIO_UNIT_PROPERTY_SET_RENDER_CALLBACK: u32 = 23;
const K_AUDIO_UNIT_SCOPE_INPUT: u32 = 1;
const K_AUDIO_UNIT_SCOPE_GLOBAL: u32 = 0;
/// The output element (bus) of an I/O unit.
const OUTPUT_BUS: u32 = 0;

#[link(name = "AudioToolbox", kind = "framework")]
extern "C" {
    fn AudioComponentFindNext(component: AudioComponent, desc: *const AudioComponentDescription) -> AudioComponent;
    fn AudioComponentInstanceNew(component: AudioComponent, out: *mut AudioUnit) -> OSStatus;
    fn AudioComponentInstanceDispose(unit: AudioUnit) -> OSStatus;
    fn AudioUnitSetProperty(
        unit: AudioUnit,
        id: u32,
        scope: u32,
        element: u32,
        data: *const c_void,
        size: u32,
    ) -> OSStatus;
    fn AudioUnitInitialize(unit: AudioUnit) -> OSStatus;
    fn AudioUnitUninitialize(unit: AudioUnit) -> OSStatus;
    fn AudioOutputUnitStart(unit: AudioUnit) -> OSStatus;
    fn AudioOutputUnitStop(unit: AudioUnit) -> OSStatus;
}

const SAMPLE_RATE: u32 = 48000;
const CHANNELS: u16 = 2;

/// The mixer currently being pulled by the render callback, if any.
static MIXER: Mutex<Option<Mixer>> = Mutex::new(None);

/// The running unit, so it can be stopped and restarted when the app goes to the background.
static UNIT: AtomicUsize = AtomicUsize::new(0);

/// The two reasons the output may be paused: the app in the background, or another app (a call,
/// an alarm) holding the audio. Kept together so neither can undo the other.
struct PausedState {
    activity: bool,
    focus: bool,
    applied: bool,
}

static PAUSED: Mutex<PausedState> = Mutex::new(PausedState { activity: false, focus: false, applied: false });

fn update_paused(activity: Option<bool>, focus: Option<bool>) {
    let (want, changed) = {
        let mut state = PAUSED.lock().unwrap();
        if let Some(value) = activity {
            state.activity = value;
        }
        if let Some(value) = focus {
            state.focus = value;
        }
        let want = state.activity || state.focus;
        let changed = want != state.applied;
        state.applied = want;
        (want, changed)
    };
    if changed {
        apply_paused(want);
    }
}

fn apply_paused(paused: bool) {
    let unit = UNIT.load(Ordering::Relaxed) as AudioUnit;
    if unit.is_null() {
        return
    }
    unsafe {
        let status = if paused { AudioOutputUnitStop(unit) } else { AudioOutputUnitStart(unit) };
        if status != 0 {
            eprintln!("[coreaudio] {} fallo (OSStatus {})", if paused { "parar" } else { "reanudar" }, status);
        }
    }
}

/// Called when the app goes to the background and comes back.
pub fn set_activity_paused(paused: bool) {
    update_paused(Some(paused), None);
}

/// Called when another app takes the audio (a call, an alarm) and when it gives it back.
pub fn set_focus_paused(paused: bool) {
    update_paused(None, Some(paused));
}

/// Set once the mixer has produced something audible.
static SOUND: AtomicBool = AtomicBool::new(false);

/// Whether anything has actually been heard yet.
pub fn sound_started() -> bool {
    SOUND.load(Ordering::Relaxed)
}

/// Pulls `frames` of interleaved f32 audio out of the mixer. Runs on the real-time audio thread,
/// so a lock that is taken right now yields silence instead of blocking it.
extern "C" fn render_callback(
    _ref_con: *mut c_void,
    _action_flags: *mut u32,
    _time_stamp: *const c_void,
    _bus_number: u32,
    frames: u32,
    data: *mut AudioBufferList,
) -> OSStatus {
    if data.is_null() {
        return 0
    }
    // SAFETY: the unit hands over one interleaved buffer, in the format set in `create_unit`.
    let buffer = unsafe {
        let list = &mut *data;
        if list.number_buffers == 0 || list.buffers[0].data.is_null() {
            return 0
        }
        let available = list.buffers[0].data_byte_size as usize / std::mem::size_of::<Sample>();
        let wanted = frames as usize * usize::from(CHANNELS);
        std::slice::from_raw_parts_mut(list.buffers[0].data as *mut Sample, available.min(wanted))
    };
    match MIXER.try_lock() {
        Ok(mut guard) => match guard.as_mut() {
            Some(mixer) => {
                let count = mixer.write_samples(buffer);
                if count < buffer.len() {
                    buffer[count..].iter_mut().for_each(|x| *x = 0.0);
                }
            },
            None => buffer.iter_mut().for_each(|x| *x = 0.0),
        },
        Err(_) => buffer.iter_mut().for_each(|x| *x = 0.0),
    }
    if !SOUND.load(Ordering::Relaxed) && buffer.iter().any(|x| *x != 0.0) {
        SOUND.store(true, Ordering::Relaxed);
    }
    0
}

fn check(status: OSStatus, what: &str) -> Result<(), String> {
    if status == 0 {
        Ok(())
    } else {
        Err(format!("{} fallo (OSStatus {})", what, status))
    }
}

/// Builds, configures and initialises the RemoteIO unit. The caller owns it.
unsafe fn create_unit() -> Result<AudioUnit, String> {
    let desc = AudioComponentDescription {
        component_type: K_AUDIO_UNIT_TYPE_OUTPUT,
        component_sub_type: K_AUDIO_UNIT_SUB_TYPE_REMOTE_IO,
        component_manufacturer: K_AUDIO_UNIT_MANUFACTURER_APPLE,
        component_flags: 0,
        component_flags_mask: 0,
    };
    let component = AudioComponentFindNext(std::ptr::null_mut(), &desc);
    if component.is_null() {
        return Err("no hay unidad RemoteIO".into())
    }
    let mut unit: AudioUnit = std::ptr::null_mut();
    check(AudioComponentInstanceNew(component, &mut unit), "AudioComponentInstanceNew")?;

    let bytes_per_frame = (std::mem::size_of::<Sample>() * usize::from(CHANNELS)) as u32;
    let format = AudioStreamBasicDescription {
        sample_rate: f64::from(SAMPLE_RATE),
        format_id: K_AUDIO_FORMAT_LINEAR_PCM,
        format_flags: K_AUDIO_FORMAT_FLAG_IS_FLOAT | K_AUDIO_FORMAT_FLAG_IS_PACKED,
        bytes_per_packet: bytes_per_frame,
        frames_per_packet: 1,
        bytes_per_frame,
        channels_per_frame: u32::from(CHANNELS),
        bits_per_channel: (std::mem::size_of::<Sample>() * 8) as u32,
        reserved: 0,
    };
    let callback = AURenderCallbackStruct { input_proc: render_callback, input_proc_ref_con: std::ptr::null_mut() };
    let configured = check(
        AudioUnitSetProperty(
            unit,
            K_AUDIO_UNIT_PROPERTY_STREAM_FORMAT,
            K_AUDIO_UNIT_SCOPE_INPUT,
            OUTPUT_BUS,
            (&format as *const AudioStreamBasicDescription).cast(),
            std::mem::size_of::<AudioStreamBasicDescription>() as u32,
        ),
        "formato de audio",
    )
    .and_then(|()| {
        check(
            AudioUnitSetProperty(
                unit,
                K_AUDIO_UNIT_PROPERTY_SET_RENDER_CALLBACK,
                K_AUDIO_UNIT_SCOPE_GLOBAL,
                OUTPUT_BUS,
                (&callback as *const AURenderCallbackStruct).cast(),
                std::mem::size_of::<AURenderCallbackStruct>() as u32,
            ),
            "callback de audio",
        )
    })
    .and_then(|()| check(AudioUnitInitialize(unit), "AudioUnitInitialize"));
    if let Err(error) = configured {
        AudioComponentInstanceDispose(unit);
        return Err(error)
    }
    Ok(unit)
}

unsafe fn dispose_unit(unit: AudioUnit) {
    AudioOutputUnitStop(unit);
    AudioUnitUninitialize(unit);
    AudioComponentInstanceDispose(unit);
}

/// Closes the unit owned by a finished game session, unless a newer session already replaced it.
pub fn close_stream(stream: usize) {
    if stream == 0 {
        return
    }
    if UNIT.compare_exchange(stream, 0, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
        unsafe { dispose_unit(stream as AudioUnit) };
    }
}

/// Opens the output and hands the mixer over to its callback. Returns the unit (as a number, to
/// match the Android version), the channel count, the sample rate, and the handle used to add sounds.
pub fn open(
    global_volume: Arc<AtomicU32>,
    gb_audio: Arc<GbAudioParams>,
) -> Result<(usize, ChannelCount, SampleRate, MixerHandle), String> {
    unsafe {
        // A new game session in the same process replaces the old output, so two units never pull
        // from the mixer at once.
        let previous = UNIT.swap(0, Ordering::SeqCst) as AudioUnit;
        if !previous.is_null() {
            dispose_unit(previous);
        }
        if let Ok(mut mixer) = MIXER.lock() {
            *mixer = None;
        }

        let channels = ChannelCount::new(CHANNELS).ok_or("canal de audio invalido")?;
        let sample_rate = SampleRate::new(SAMPLE_RATE).ok_or("frecuencia de audio invalida")?;
        let (mixer, handle) = Mixer::new(sample_rate, channels, global_volume, gb_audio);
        *MIXER.lock().unwrap() = Some(mixer);

        let unit = create_unit()?;
        UNIT.store(unit as usize, Ordering::SeqCst);
        let paused = PAUSED.lock().map(|state| state.applied).unwrap_or(false);
        if !paused {
            check(AudioOutputUnitStart(unit), "AudioOutputUnitStart")?;
        }
        eprintln!("[coreaudio] salida abierta: {} canales @ {} Hz", CHANNELS, SAMPLE_RATE);
        Ok((unit as usize, channels, sample_rate, handle))
    }
}
