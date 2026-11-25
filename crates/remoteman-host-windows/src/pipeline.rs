//! The picture path: the driver's shared texture → one Direct3D conversion → the encoder → the
//! client. One thread does all of it, because the device's context is one thread's at a time
//! and the encoder works on the same device. Sending waits while the network is behind, and
//! while it waits the driver goes on replacing the frame in its channel with newer ones: frames
//! are skipped before encoding, never after (every frame refers to the one before).
//!
//! No frame, no send: a still desktop costs nothing. The last converted picture is kept in the
//! encoder's input, so a keyframe or a recovery frame can be made from it when a client asks
//! and nothing on the desktop has changed.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use remoteman_core::host::{Accept, Link};
use remoteman_core::{Chroma, Codec, Message, Mode};
use remoteman_encode::FrameEncoder;
use remoteman_encode::recovery::{Recovery, Sent};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_FAILED};
use windows::Win32::System::Performance::QueryPerformanceCounter;
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForMultipleObjects};

use windows::Win32::Graphics::Direct3D11::ID3D11ShaderResourceView;

use crate::capture::{Gpu, Source, Take};
use crate::convert::{self, Converter, Output};
use crate::display;
use crate::mirror::Duplication;
use crate::encode;
use crate::vdd::{Driver, Metadata};

pub struct Settings {
    pub driver: Arc<Driver>,
    /// Stream this screen of the PC (Windows' name for it, `\.\DISPLAY1`) as it is, through
    /// Desktop Duplication, instead of the driver's monitor.
    pub duplicate: Option<String>,
    /// The driver's number for the monitor to stream.
    pub monitor: u32,
    /// The modes the client decodes and this host encodes, the client's favourite first. The
    /// first one this GPU's encoder opens for is used.
    pub modes: Vec<Mode>,
    pub fps: u32,
    pub bitrate: u32,
    /// Where on the desktop to draw a dot for a moment, to make Windows show a frame when the
    /// channel has none yet.
    pub nudge_at: (i32, i32),
    /// The size the monitor was just brought to, if it was: the channel of that size is the one
    /// to wait for, since for a moment after a mode change the driver still has the old one.
    pub expect: Option<(u32, u32)>,
    /// Send nothing until [`Pipeline::release`]: for a picture that replaces another, which the
    /// client must be told of before its first frame.
    pub held: bool,
    /// Tell the client what the picture is encoded at (the first picture's business only).
    pub status: bool,
    /// The client asked for HDR: BT.2020 PQ if the encoder can say so.
    pub hdr: bool,
    /// Draw the pointer into the picture, as the cursor's thread says it is (for a client that
    /// doesn't draw it, and for a second display).
    pub pointer: Option<Arc<crate::cursor::Drawn>>,
    /// SDR white on the monitor, in nits (Windows' "SDR content brightness"): where an SDR
    /// desktop's white goes in an HDR picture, and what an HDR desktop's SDR white is.
    pub sdr_white: f32,
    /// Called when the picture's size changes under a running stream (someone changed the
    /// monitor's mode), after the client has been told.
    pub resized: Box<dyn Fn(u32, u32) + Send>,
}

/// What the picture turned out to be.
#[derive(Debug, Clone, Copy)]
pub struct Started {
    pub mode: Mode,
    pub width: u32,
    pub height: u32,
    /// BT.2020 PQ: SDR white and the monitor's peak, in nits. `None`: BT.709.
    pub hdr: Option<(u32, u32)>,
}

impl Started {
    /// What the client is told of this picture.
    pub fn accept(&self, refresh: u32) -> Accept {
        let (colour, (sdr_white, peak)) = match self.hdr {
            Some(light) => ("bt2020-pq", light),
            None => ("bt709", (0, 0)),
        };
        Accept { mode: self.mode, width: self.width, height: self.height, refresh, colour: colour.into(), sdr_white, peak }
    }
}

/// An event the picture's thread waits on beside the driver's.
struct Wake(HANDLE);
// SAFETY: an event handle; setting it is allowed from any thread.
unsafe impl Send for Wake {}
unsafe impl Sync for Wake {}
impl Drop for Wake {
    fn drop(&mut self) {
        // SAFETY: ours, and not used again.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

struct Control {
    stop: AtomicBool,
    held: AtomicBool,
    keyframe: AtomicBool,
    /// A loss report not yet acted on: the first frame the client missed, and the newest it
    /// had seen when it said so.
    lost: Mutex<Option<(u32, u32)>>,
    /// The bitrate wanted, in bits a second; the encoder follows it at its next turn.
    bitrate: AtomicU32,
    wake: Wake,
}

impl Control {
    fn wake(&self) {
        // SAFETY: a live event.
        let _ = unsafe { SetEvent(self.wake.0) };
    }
}

pub struct Pipeline {
    control: Arc<Control>,
    thread: Option<JoinHandle<()>>,
}

impl Pipeline {
    /// Start streaming the driver's monitor to `link`. Returns once the first picture's size
    /// and mode are known (or it is clear there will be none).
    pub fn start(settings: Settings, link: Link) -> Result<(Self, Started)> {
        // SAFETY: an unnamed event that resets itself.
        let wake = Wake(unsafe { CreateEventW(None, false, false, None) }.context("CreateEvent")?);
        let control = Arc::new(Control { stop: AtomicBool::new(false), held: AtomicBool::new(settings.held), keyframe: AtomicBool::new(true), lost: Mutex::new(None), bitrate: AtomicU32::new(settings.bitrate), wake });
        let (ready_tx, ready_rx) = mpsc::channel::<Result<Started>>();
        if let Some(drawn) = &settings.pointer {
            let c = control.clone();
            drawn.set_wake(Box::new(move || c.wake()));
        }
        let c = control.clone();
        let thread = std::thread::Builder::new().name("picture".into()).spawn(move || {
            let picture = match Picture::open(&settings) {
                Ok(p) => {
                    let _ = ready_tx.send(Ok(Started { mode: p.mode, width: p.width, height: p.height, hdr: p.hdr }));
                    p
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            if let Err(e) = run(picture, &settings, &link, &c) {
                tracing::error!("the picture stopped: {e:#}");
            }
        })?;
        let started = ready_rx.recv().context("the picture's thread died")??;
        Ok((Self { control, thread: Some(thread) }, started))
    }

    /// Let a pipeline that was started held back send.
    pub fn release(&self) {
        self.control.held.store(false, Ordering::Relaxed);
        self.control.wake();
    }

    /// Make the next frame a keyframe.
    pub fn request_keyframe(&self) {
        self.control.keyframe.store(true, Ordering::Relaxed);
        self.control.wake();
    }

    /// Encode at this many bits a second from now on.
    pub fn set_bitrate(&self, bitrate: u32) {
        self.control.bitrate.store(bitrate, Ordering::Relaxed);
        self.control.wake();
    }

    /// The client missed frames from `first` on, and had seen up to `last` when it said so:
    /// recover at the next frame.
    pub fn report_lost(&self, first: u32, last: u32) {
        {
            // Of reports not yet acted on, keep the earliest loss and the newest sighting.
            let mut lost = self.control.lost.lock().unwrap();
            *lost = Some(lost.map_or((first, last), |(f, l)| (f.min(first), l.max(last))));
        }
        self.control.wake();
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        self.control.stop.store(true, Ordering::Relaxed);
        self.control.wake();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Where the frames come from: the driver's channel, or one of the PC's screens.
enum Frames {
    Driver(Source),
    Screen(Duplication),
}

impl Frames {
    fn meta(&self) -> Metadata {
        match self {
            Frames::Driver(s) => s.meta,
            Frames::Screen(d) => d.meta,
        }
    }

    fn take<R>(&mut self, gpu: &Gpu, again: bool, read: impl FnOnce(&ID3D11ShaderResourceView, &Metadata) -> R) -> Result<Take<R>> {
        match self {
            Frames::Driver(s) => s.take(gpu, again, read),
            // (Waits for the desktop to change, a few milliseconds at most, so that the
            // thread's other business is seen to soon.)
            Frames::Screen(d) => d.take(gpu, again, Duration::from_millis(8), read),
        }
    }

    /// The driver's counts, or the frames taken of a screen.
    fn counts(&self) -> Option<(u64, u64, u64, u64)> {
        match self {
            Frames::Driver(s) => s.counts(),
            Frames::Screen(d) => Some((d.frames(), 0, 0, 0)),
        }
    }
}

/// Everything one picture is made with. In this order because they are dropped in it: the
/// encoder before the texture it reads, and all of them before the device.
struct Picture {
    encoder: Box<dyn FrameEncoder>,
    recovery: Recovery,
    converter: Converter,
    source: Frames,
    gpu: Gpu,
    mode: Mode,
    width: u32,
    height: u32,
    /// BT.2020 PQ (SDR white and peak, in nits), or BT.709.
    hdr: Option<(u32, u32)>,
    /// The encoder's input holds a picture: there is something to encode again.
    have: bool,
    /// Take the newest frame even if it was taken before (a new client, or a new channel).
    again: bool,
}

/// The driver's frame channel, waiting for it if it isn't there yet: the driver makes it at
/// the first frame after it was first asked about, so the desktop is made to draw something.
fn wait_for_channel(s: &Settings, patience: Duration) -> Result<crate::vdd::Channel> {
    let began = Instant::now();
    let mut tries = 0u32;
    loop {
        if let Some(channel) = display::channel(&s.driver, s.monitor)? {
            let size = channel.snapshot().map(|m| (m.width, m.height));
            // Not the size the monitor is in now: the channel of the swap chain Windows is
            // about to take away. Give the new one three seconds, then take what there is.
            if s.expect.is_none() || size == s.expect || began.elapsed() > Duration::from_secs(3) {
                if tries > 0 {
                    tracing::info!(after_ms = began.elapsed().as_millis() as u64, tries, ?size, "the frame channel is there");
                }
                return Ok(channel);
            }
        }
        if tries == 0 {
            tracing::info!("the driver has no frame channel yet (it makes one at the next frame Windows shows); making the desktop draw");
        }
        ensure!(began.elapsed() < patience, "the driver's frame channel didn't appear in {} s: no frame was shown on its monitor (is the monitor part of the desktop, and is this process in the desktop's session?)", patience.as_secs());
        tries += 1;
        display::nudge(s.nudge_at);
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// A converter and an encoder for the picture `meta` describes, in the first of `modes` the
/// encoder opens for, and HDR if asked and the encoder can say so.
fn build(gpu: &Gpu, meta: &Metadata, s: &Settings, bitrate: u32) -> Result<(Converter, Box<dyn FrameEncoder>, Mode, Option<(u32, u32)>)> {
    let (width, height) = (meta.width, meta.height);
    ensure!(width % 2 == 0 && height % 2 == 0 && width >= 64 && height >= 64, "the monitor is {width}x{height}; sizes must be even and at least 64");
    let (input, note) = convert::input_for(meta.format, meta.is_hdr != 0, s.sdr_white)?;
    if let Some(note) = note {
        tracing::warn!("{note}");
    }
    let hdr = s.hdr && encode::can_hdr(gpu);
    if s.hdr && !hdr {
        tracing::warn!("the client asked for HDR, which this GPU's encoder isn't told how to say: the picture is SDR");
    }
    let output = if hdr { Output::Pq { sdr_white: s.sdr_white } } else { Output::Sdr };
    // The monitor's peak as Windows was told it (its EDID's), else PQ's usual 1000.
    let light = hdr.then(|| (s.sdr_white.round() as u32, if meta.max_nits > 0.0 { meta.max_nits.round() as u32 } else { 1000 }));
    let light_sei = hdr.then(|| encode::hdr_light(meta, s.sdr_white));
    // AV1 gets no metadata OBUs: on 2026-11-20 the iPad's VideoToolbox AV1 decoder gave no picture,
    // and no error, for samples holding them (the Mac's took them), so an iPad app that passes
    // them on stays black. Devices have the light levels from the stream message anyway.
    // REMOTEMAN_AV1_HDR_METADATA=1 sends them.
    let av1_light = std::env::var("REMOTEMAN_AV1_HDR_METADATA").as_deref() == Ok("1");
    let mut refused = Vec::new();
    for &mode in &s.modes {
        let converter = Converter::new(gpu, width, height, input, output, mode.chroma == Chroma::C444)?;
        match encode::open(gpu, &converter.output, &encode::Settings { mode, width, height, fps: s.fps, bitrate, hdr, light: if mode.codec == Codec::Av1 && !av1_light { None } else { light_sei } }) {
            Ok(encoder) => {
                tracing::info!(?mode, width, height, fps = s.fps, bitrate, format = meta.format, hdr = meta.is_hdr, ?input, ?output, "encoder ready ({})", encoder.note());
                return Ok((converter, encoder, mode, light));
            }
            Err(e) => {
                tracing::warn!(?mode, "{e:#}");
                refused.push(format!("{mode:?}: {e:#}"));
            }
        }
    }
    bail!("no encoder opened: {}", refused.join("; "))
}

impl Picture {
    fn open(s: &Settings) -> Result<Self> {
        if let Some(device) = &s.duplicate {
            let gpu = Gpu::on_adapter(crate::mirror::adapter_of(device)?)?;
            let screen = Duplication::open(&gpu, device)?;
            let meta = screen.meta;
            let (converter, encoder, mode, hdr) = build(&gpu, &meta, s, s.bitrate)?;
            let recovery = Recovery::new(encoder.short_term(), encoder.long_term(), s.fps);
            return Ok(Self { encoder, recovery, converter, gpu, mode, width: meta.width, height: meta.height, hdr, source: Frames::Screen(screen), have: false, again: true });
        }
        let channel = wait_for_channel(s, Duration::from_secs(6))?;
        let meta = channel.snapshot().context("the frame channel's metadata isn't readable (never whole, or not this driver's)")?;
        tracing::info!(
            width = meta.width,
            height = meta.height,
            format = meta.format,
            hdr = meta.is_hdr,
            slots = channel.slots.len(),
            frames = meta.frame_counter,
            generation = meta.generation(),
            flags = format!("{:#x}", channel.flags),
            "frame channel"
        );
        let gpu = Gpu::on_adapter((meta.luid_low, meta.luid_high))?;
        let source = Source::open(&gpu, channel)?;
        let (converter, encoder, mode, hdr) = build(&gpu, &source.meta, s, s.bitrate)?;
        let recovery = Recovery::new(encoder.short_term(), encoder.long_term(), s.fps);
        Ok(Self { encoder, recovery, converter, gpu, mode, width: source.meta.width, height: source.meta.height, hdr, source: Frames::Driver(source), have: false, again: true })
    }

    /// The channel in hand is dead (or may be): take the driver's current one. If the picture
    /// is another size or format now, the stream starts afresh as that. False: the driver has
    /// no channel at the moment (between two modes); try again later.
    fn reopen(&mut self, s: &Settings, link: &Link, bitrate: u32) -> Result<bool> {
        let source = match &s.duplicate {
            Some(device) => match Duplication::open(&self.gpu, device) {
                Ok(d) => Frames::Screen(d),
                Err(e) => {
                    tracing::debug!("the screen can't be duplicated yet: {e:#}");
                    return Ok(false);
                }
            },
            None => {
                let Some(channel) = display::channel(&s.driver, s.monitor)? else { return Ok(false) };
                Frames::Driver(Source::open(&self.gpu, channel)?)
            }
        };
        let (old, new) = (self.source.meta(), source.meta());
        if (old.width, old.height, old.format, old.is_hdr) != (new.width, new.height, new.format, new.is_hdr) {
            tracing::info!(from = format!("{}x{}", old.width, old.height), to = format!("{}x{}", new.width, new.height), format = new.format, hdr = new.is_hdr, "the monitor changed; the picture starts afresh");
            // Only this mode: the client has its decoder set up for it.
            let only = Settings { driver: s.driver.clone(), duplicate: s.duplicate.clone(), monitor: s.monitor, modes: vec![self.mode], fps: s.fps, bitrate, nudge_at: s.nudge_at, expect: None, held: false, status: false, hdr: s.hdr, pointer: None, sdr_white: s.sdr_white, resized: Box::new(|_, _| {}) };
            let (converter, encoder, mode, hdr) = build(&self.gpu, &new, &only, bitrate)?;
            // The old encoder goes before the texture it reads from.
            self.encoder = encoder;
            self.converter = converter;
            self.recovery = Recovery::new(self.encoder.short_term(), self.encoder.long_term(), s.fps);
            (self.width, self.height, self.have, self.hdr) = (new.width, new.height, false, hdr);
            if !link.restart(Started { mode, width: new.width, height: new.height, hdr }.accept(s.fps)) {
                bail!("the client is gone");
            }
            (s.resized)(new.width, new.height);
        } else {
            tracing::info!("the driver's frame channel was replaced; carrying on with the new one");
        }
        self.source = source;
        self.again = true;
        Ok(true)
    }

    /// Open the encoder again at `bitrate`, on the same converter, for the same picture and
    /// mode: for an encoder that can't change its rate live (Direct3D 12 on Intel's Arc B580).
    /// The client's decoder carries on; the next frame has to be a keyframe.
    fn reopen_encoder(&mut self, s: &Settings, bitrate: u32) -> Result<()> {
        let meta = self.source.meta();
        let hdr = self.hdr.is_some();
        let av1_light = std::env::var("REMOTEMAN_AV1_HDR_METADATA").as_deref() == Ok("1");
        let light = (hdr && (self.mode.codec != Codec::Av1 || av1_light)).then(|| encode::hdr_light(&meta, s.sdr_white));
        let settings = encode::Settings { mode: self.mode, width: self.width, height: self.height, fps: s.fps, bitrate, hdr, light };
        let encoder = encode::open(&self.gpu, &self.converter.output, &settings)?;
        tracing::info!(bitrate, "encoder opened again at the new rate ({})", encoder.note());
        self.encoder = encoder;
        self.recovery.renewed(self.encoder.short_term(), self.encoder.long_term(), s.fps);
        Ok(())
    }
}

/// An encoder that can't change its rate live is opened again at the new one, from a keyframe:
/// at once when the rate goes down (the network is short), and on the way back up only once the
/// rate asked for has held this long or grown by half, so that the core's small steps up don't
/// each cost a keyframe and a new encoder.
const REOPEN_UP_AFTER: Duration = Duration::from_secs(3);

/// How long ago Windows meant a frame to be shown, in microseconds; 0 if that is still to come
/// or makes no sense.
fn age_us(meta: &Metadata) -> u64 {
    let mut now = 0i64;
    // SAFETY: a live out-parameter.
    if unsafe { QueryPerformanceCounter(&mut now) }.is_err() || meta.qpc_frequency == 0 || meta.present_qpc == 0 {
        return 0;
    }
    let ticks = (now as u64).saturating_sub(meta.present_qpc);
    let us = ticks.saturating_mul(1_000_000) / meta.qpc_frequency;
    if us > 100_000 { 0 } else { us }
}

/// Static refinement (`--refine`): when the picture has been still this long, it is encoded once
/// more with room for [`REFINE_FACTOR`] times the bits, so text and fine detail stop being as
/// soft as the last frame of motion. The next ordinary frame has the rate back.
pub static REFINE: AtomicBool = AtomicBool::new(true);
const REFINE_AFTER: Duration = Duration::from_millis(400);
const REFINE_FACTOR: u32 = 8;
/// The refining frame's constant quantiser when `--refine-qp` gives one (1 to 51); 0 chooses it
/// by the picture's size ([`refine_qp`]).
pub static REFINE_QP: AtomicU32 = AtomicU32::new(0);
/// The constant quantiser of the refining frame: finer for small pictures, coarser for large ones
/// so the one frame stays within a few megabytes even for a page of dense text (measured on a
/// worst-case 720p text pattern: QP 24 is 450 kB, 28 is 360 kB, 16 is 630 kB). A 1440p monitor,
/// ultrawide (3440x1440, 4.95 megapixels) or 2560x1600, keeps QP 24, about 2.4 MB for that page:
/// at 32, which it had before, the still picture came out softer than the frames before it, and
/// text in full screen went blurry the moment it stopped moving. 4K gets 28, larger 32.
fn refine_qp(width: u32, height: u32) -> u32 {
    let given = REFINE_QP.load(Ordering::Relaxed);
    if given > 0 {
        return given;
    }
    match width as u64 * height as u64 {
        0..=5_300_000 => 24,
        5_300_001..=9_000_000 => 28,
        _ => 32,
    }
}

fn run(mut p: Picture, s: &Settings, link: &Link, control: &Control) -> Result<()> {
    let mut rate = s.bitrate;
    // The rate last asked for, and since when; how often the encoder was opened again for one.
    let (mut asked, mut reopens) = ((rate, Instant::now()), 0u64);
    let (mut frames, mut keys, mut repeats) = (0u64, 0u64, 0u64);
    // What the encoder made of its rate: bytes in all, the largest frame, and its quantiser
    // summed over the frames it said one for (for the average) and at the last frame.
    let (mut bytes, mut largest, mut qp_sum, mut qp_said, mut qp_last) = (0u64, 0usize, 0u64, 0u64, 0u32);
    let began = Instant::now();
    // How long each new picture took from being taken up (converted) to being encoded, in
    // milliseconds; said every ten seconds and at the end.
    let mut took: Vec<f32> = Vec::new();
    let mut took_all: Vec<f32> = Vec::new();
    let mut said_at = Instant::now();
    let mut taken_at = None::<Instant>;
    // When a frame last came, and when the channel was last checked for having been replaced.
    let (mut quiet_since, mut checked) = (Instant::now(), Instant::now());
    // The stream must begin with a keyframe after it starts afresh.
    let mut afresh = false;
    let mut last_nudge = Instant::now();
    let mut wait_failed = false;
    // No more frames a second than the client was told (a monitor that is mirrored, or that
    // doesn't offer the client's rate, shows more): a frame that comes too soon is held back
    // until its turn, and replaced by a newer one if that comes first. The encoder's rate
    // control counts on the rate it was told.
    let interval = Duration::from_secs_f64(1.0 / s.fps.max(1) as f64);
    let mut last_sent = Instant::now() - interval;
    let mut held_back = None::<u64>;
    let mut told_rate = false;
    // The still picture has had its refining frame (until the next new picture).
    let mut refined = false;
    let mut refines = 0u64;
    // The pointer's last change drawn in (none yet).
    let mut pointer_seen = u64::MAX;
    loop {
        let held = control.held.load(Ordering::Relaxed);
        if !p.again || held {
            // Held back: wait for the word only, and leave the driver's frames where they are.
            let ready = match &p.source {
                Frames::Driver(source) => Some(source.channel().ready),
                Frames::Screen(_) => None,
            };
            let handles = [control.wake.0, ready.unwrap_or_default()];
            // SAFETY: live events. The timeout is for the housekeeping below.
            let patience = match held_back {
                Some(_) => interval.saturating_sub(last_sent.elapsed()).as_millis().clamp(1, 100) as u32,
                None => 100,
            };
            // A screen has no event to wait on: asking for its frames waits.
            let patience = if ready.is_none() && !held { 0 } else { patience };
            let waited = unsafe { WaitForMultipleObjects(&handles[..if held || ready.is_none() { 1 } else { 2 }], false, patience) };
            if waited == WAIT_FAILED {
                // Not to become a loop that eats a core in silence.
                if !std::mem::replace(&mut wait_failed, true) {
                    tracing::warn!("waiting for the driver's frame event failed ({}); looking for frames ten times a second instead. Said once.", windows::core::Error::from_thread());
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        if control.stop.load(Ordering::Relaxed) {
            break;
        }
        if control.held.load(Ordering::Relaxed) {
            continue;
        }
        // A new bitrate takes effect here, with no keyframe; or, where the encoder can't change
        // it live, by opening the encoder again at it (`REOPEN_UP_AFTER`).
        let want = control.bitrate.load(Ordering::Relaxed);
        if want != asked.0 {
            asked = (want, Instant::now());
        }
        if want != rate {
            match p.encoder.set_bitrate(want) {
                Ok(()) => {
                    tracing::info!(from = rate, to = want, "bitrate");
                    rate = want;
                    if s.status {
                        link.send_message(Message::Status { bitrate: Some(rate) });
                    }
                }
                Err(live) if want < rate || asked.1.elapsed() >= REOPEN_UP_AFTER || want as u64 * 2 >= rate as u64 * 3 => match p.reopen_encoder(s, want) {
                    Ok(()) => {
                        tracing::info!(from = rate, to = want, "bitrate (the encoder can't change it live: {live:#}; opened again, from a keyframe)");
                        rate = want;
                        reopens += 1;
                        control.keyframe.store(true, Ordering::Relaxed);
                        if s.status {
                            link.send_message(Message::Status { bitrate: Some(rate) });
                        }
                    }
                    Err(e) => {
                        tracing::warn!("couldn't change the bitrate: {live:#}; nor open the encoder again at it: {e:#}");
                        control.bitrate.store(rate, Ordering::Relaxed);
                        asked = (rate, Instant::now());
                    }
                },
                // On the way up: wait until the rate asked for holds still.
                Err(_) => {}
            }
        }

        // The pointer moved or changed: the last frame again, with the pointer where it is now.
        if let Some(drawn) = &s.pointer {
            let now = drawn.now.lock().unwrap();
            if now.serial != pointer_seen {
                pointer_seen = now.serial;
                p.converter.set_pointer(&p.gpu, &now);
                if p.have {
                    p.again = true;
                }
            }
        }
        let again = std::mem::take(&mut p.again);
        let before = Instant::now();
        let taken = {
            let Picture { source, converter, gpu, .. } = &mut p;
            source.take(gpu, again, |view, meta| {
                converter.convert(gpu, view);
                link.now_us().saturating_sub(age_us(meta))
            })?
        };
        let stamp = match taken {
            Take::Frame(us) => {
                p.have = true;
                refined = false;
                quiet_since = Instant::now();
                taken_at = Some(before);
                Some(us)
            }
            // Asked for the newest frame whatever its state and didn't get it: the driver
            // is writing into that very texture. Ask again in a moment.
            Take::Nothing if again => {
                p.again = true;
                std::thread::sleep(Duration::from_millis(5));
                None
            }
            Take::Nothing => None,
            Take::Stale => {
                afresh |= p.reopen(s, link, rate)?;
                checked = Instant::now();
                // A new converter: the pointer goes into it afresh.
                pointer_seen = u64::MAX;
                None
            }
        };
        // Windows gives the monitor a new swap chain on a mode change and at other times of its
        // own choosing, and the driver then makes a new channel without a word in the old one.
        // So when frames stop, ask now and then whether the channel is still the driver's.
        if stamp.is_none() && matches!(p.source, Frames::Driver(_)) && quiet_since.elapsed() > Duration::from_secs(1) && checked.elapsed() > Duration::from_secs(1) {
            checked = Instant::now();
            match display::channel(&s.driver, s.monitor) {
                Ok(Some(current)) if !matches!(&p.source, Frames::Driver(d) if current.same_as(d.channel())) => {
                    drop(current);
                    afresh |= p.reopen(s, link, rate)?;
                }
                Ok(_) => {}
                Err(e) => tracing::debug!("asking after the frame channel: {e:#}"),
            }
        }
        // A screen that doesn't change gives no first picture (Desktop Duplication shows what
        // changes): make Windows draw something, as is done for the driver's monitor.
        if !p.have && matches!(p.source, Frames::Screen(_)) && last_nudge.elapsed() > Duration::from_millis(700) && quiet_since.elapsed() > Duration::from_millis(300) {
            last_nudge = Instant::now();
            tracing::info!("no first picture of the screen yet; making the desktop draw");
            display::nudge(s.nudge_at);
        }
        if afresh && p.have {
            control.keyframe.store(true, Ordering::Relaxed);
            *control.lost.lock().unwrap() = None;
            afresh = false;
        }
        if !p.have {
            continue;
        }

        // Encode when there is a new picture, or when the client needs an answer (a keyframe,
        // a loss) and the desktop is still: then the last picture is encoded again.
        let asked = control.keyframe.load(Ordering::Relaxed) || control.lost.lock().unwrap().is_some();
        // (Counted from when a frame was taken up, not from when it had been sent: encoding
        // and sending take a few milliseconds that the next frame doesn't wait for.)
        let turn = Instant::now();
        let stamp = match stamp.or(held_back.take()) {
            Some(us) if !asked && turn.duration_since(last_sent) < interval.mul_f32(0.6) => {
                held_back = Some(us);
                None
            }
            other => other,
        };
        // Still for a moment, nobody asking for anything, no frame waiting its turn: the last
        // picture once more, finer. Once per still period.
        let refining = stamp.is_none() && !asked && !refined && held_back.is_none() && REFINE.load(Ordering::Relaxed) && quiet_since.elapsed() >= REFINE_AFTER && p.encoder.can_refine();
        let capture_us = match stamp {
            Some(us) => us,
            None if asked => {
                repeats += 1;
                link.now_us()
            }
            None if refining => link.now_us(),
            None => continue,
        };
        if refining {
            refined = true;
        }
        let idr = control.keyframe.swap(false, Ordering::Relaxed);
        let reported = control.lost.lock().unwrap().take();
        let (p_mode, p_hdr, p_size) = (p.mode, p.hdr.is_some(), (p.width, p.height));
        let Picture { encoder, recovery, .. } = &mut p;
        let n = recovery.number();
        let plan = recovery.plan(idr, reported, &mut |t| encoder.invalidate(t).inspect_err(|e| tracing::debug!(t, "invalidate: {e:#}")));
        if reported.is_some() {
            tracing::debug!(?reported, n, ?plan, "loss report");
        }
        let made = if refining { encoder.encode_refined(0, n, plan.how, REFINE_FACTOR, refine_qp(p_size.0, p_size.1)) } else { encoder.encode_how(0, n, plan.how) }.context("encode")?;
        if refining {
            refines += 1;
            tracing::info!(bytes = made.data.len(), qp = made.qp, "refined the still picture");
        }
        if let Some(at) = taken_at.take() {
            took.push(at.elapsed().as_secs_f32() * 1000.0);
        }
        if said_at.elapsed() >= Duration::from_secs(10) && !took.is_empty() {
            took.sort_by(f32::total_cmp);
            tracing::info!(pictures = took.len(), median_ms = format!("{:.2}", took[took.len() / 2]), p99_ms = format!("{:.2}", took[took.len() * 99 / 100]), worst_ms = format!("{:.2}", took[took.len() - 1]), mode = ?p_mode, hdr = p_hdr, "convert and encode, in the last ten seconds");
            took_all.append(&mut took);
            said_at = Instant::now();
        }
        let sent = recovery.made(&plan, made.key, made.marked, made.referred);
        frames += 1;
        keys += made.key as u64;
        bytes += made.data.len() as u64;
        largest = largest.max(made.data.len());
        if made.qp > 0 {
            qp_sum += made.qp as u64;
            qp_said += 1;
        }
        qp_last = made.qp;
        let delivered = match sent {
            Sent::Key => link.send_frame(made.data, true, capture_us),
            Sent::Recovery => link.send_recovery_frame(made.data, capture_us),
            Sent::Delta => link.send_frame(made.data, false, capture_us),
        };
        last_sent = turn;
        if !delivered {
            break;
        }
        // What the picture is encoded at, said once it has begun (viewers show it) and again
        // whenever it changes.
        if s.status && !std::mem::replace(&mut told_rate, true) {
            link.send_message(Message::Status { bitrate: Some(rate) });
        }
    }
    took_all.append(&mut took);
    if !took_all.is_empty() {
        took_all.sort_by(f32::total_cmp);
        tracing::info!(pictures = took_all.len(), median_ms = format!("{:.2}", took_all[took_all.len() / 2]), p99_ms = format!("{:.2}", took_all[took_all.len() * 99 / 100]), mode = ?p.mode, hdr = p.hdr.is_some(), "convert and encode, over the whole picture");
    }
    let driver = p.source.counts();
    tracing::info!(
        frames,
        keyframes = keys,
        encoded_again = repeats,
        opened_again_for_rate = reopens,
        refined = refines,
        seconds = began.elapsed().as_secs(),
        kilobytes = bytes / 1000,
        mbit_per_s = format!("{:.1}", bytes as f64 * 8.0 / 1e6 / began.elapsed().as_secs_f64().max(0.001)),
        target_mbit = format!("{:.1}", rate as f64 / 1e6),
        largest_frame_bytes = largest,
        qp_average = if qp_said > 0 { format!("{:.1}", qp_sum as f64 / qp_said as f64) } else { "not said".to_owned() },
        qp_frames = qp_said,
        qp_last,
        driver_frames = driver.map(|d| d.0),
        driver_replaced_unread = driver.map(|d| d.1),
        driver_dropped_held = driver.map(|d| d.2),
        driver_dropped_failed = driver.map(|d| d.3),
        "picture over (the driver's counts are since its channel was made)"
    );
    let t = &p.recovery.tally;
    if t.reports > 0 {
        tracing::info!(
            reports = t.reports,
            recovered = t.recovered,
            long_term = t.long_term,
            early = t.early,
            too_old = t.too_old,
            refused = t.refused,
            unasked = t.unasked,
            unsure = t.unsure,
            behind = ?t.behind,
            "loss reports: answered from a recent frame, from a long-term frame, came before the last answer could arrive, too old (keyframe), refused by the encoder (keyframe); keyframes the encoder made unasked; long-term answers built from another frame than asked; how many frames behind each report was"
        );
    }
    Ok(())
}
