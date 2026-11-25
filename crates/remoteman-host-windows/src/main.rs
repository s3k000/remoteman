//! `remoteman-host`: the Windows host. It streams a monitor of the display driver to one client
//! at a time (a new one takes over) and puts the client's input into the desktop.
//!
//!     remoteman-host [--listen ADDR:PORT] [--state DIR] [--pair | --ask | --no-pair] [--name NAME]
//!                    [--log FILE] [--bitrate MBIT] [--mode keep|listed|any] [--allow files,clipboard]
//!                    [--encoder auto|nvenc|mf|d3d12] [--d3d12-av1] [--refine yes|no]
//!                    [--refine-qp QP] [--seconds N]
//!     remoteman-host --probe | --probe-picture | --probe-av1 [--log FILE]
//!     remoteman-host --probe-av1-encode DIR [--probe-size WxH]
//!     remoteman-host ctl [--port PORT] WORDS
//!     remoteman-host fingerprint [--state DIR]
//!
//! `fingerprint` prints this host's fingerprint (the SHA-256 of its certificate, in lower-case
//! hex: what a viewer checks it against) and nothing else, making its identity first if the
//! state folder has none. Its `--state` is the host's: the installed service's is
//! `C:\ProgramData\remoteman\host`, which must be said, as the default is the user's
//! `%LOCALAPPDATA%\remoteman`.
//!
//! `--no-pair`: this host never opens pairing by itself: no `--pair` or `--ask`, so only the
//! devices already in the trust list (`STATE\host\trusted`) get in, until the owner opens
//! pairing for one device with `ctl pair on` (the Pair button in Remoteman Host).
//!
//! `--ask` keeps pairing open and asks the owner, in a small window of Windows' own on the
//! desktop, about each new device that comes: its name and the six-digit code it should be
//! showing, Yes or No (No if the minute passes). This is the owner's way until the tray icon
//! is made. `--pair` trusts the first unknown device to connect, with no question. The owner's way is `ctl pair on`: the
//! device that comes then waits, `ctl status` shows it with a six-digit code that the device
//! shows too, and `ctl pair accept` or `ctl pair decline` answers it. `ctl` speaks to the
//! running host (from an elevated prompt): `status`, `pair on|off|accept|decline`, `allow
//! clipboard|files on|off`, `drop`, `revoke FINGERPRINT`.
//!
//! `--allow files,clipboard`: paired devices may put files into this user's Downloads folder,
//! and share the clipboard (text) both ways. Both are off unless said, here or with `ctl
//! allow`, which is remembered.
//!
//! `--probe` only looks: it opens the display driver, says what it finds (the driver, its frame
//! channel, the monitor's modes, the adapter, the shared textures) and exits. Run it first on
//! a new machine. `--probe-picture` goes on to take one frame, convert it and encode it once
//! with each codec, to show how far the picture path gets with no client. `--probe-av1` asks
//! every GPU's Direct3D 12 encoder what it can do for AV1 and makes each Media Foundation
//! encoder once; it needs neither the display driver nor elevation. `--probe-av1-encode DIR`
//! encodes a moving test picture (1920x1088 unless `--probe-size` says) with the Direct3D 12 AV1
//! encoder of each GPU that has one, a loss and a change of rate on the way, into
//! `DIR\av1-N.obu` for FFmpeg to decode (`ffmpeg -i av1-0.obu -f null -`).
//!
//! `--d3d12-av1` lets AV1 be encoded through Direct3D 12 (`d3d12av1.rs`), where the card's
//! encoder is Direct3D 12's (AMD's, or any with `--encoder d3d12`); `REMOTEMAN_D3D12_AV1=1` does
//! the same. Off by default until that encoder has been seen working on a card.
//!
//! `--mode` says how far the host may go to give a client its own size: `keep` streams the
//! monitor as it is, `listed` switches to the client's size if the monitor offers it, `any`
//! (the default) also has the driver add the size when it doesn't, which re-creates the monitor.
//! The monitor is put back when the client goes.
//!
//! It must run elevated (the display driver's device lets in Administrators and SYSTEM only)
//! and in the user's own desktop session (input, the cursor and mode changes work nowhere
//! else): see `input.rs`.
//!
//! The first time it listens on anything but loopback, Windows Firewall asks the user whether
//! to let it, and until the answer is "Allow" it keeps a rule that blocks it from other
//! computers. Tests on the same machine can listen on 127.0.0.1 and are never asked.
//!
//! Everything here is Windows-only; on other systems this builds to a stub so that the
//! workspace builds whole.

#[cfg(windows)]
mod advertise;
#[cfg(windows)]
mod camera;
#[cfg(windows)]
mod awake;
#[cfg(windows)]
mod speakers;
#[cfg(all(windows, test))]
mod mutetest;
#[cfg(windows)]
mod capture;
#[cfg(windows)]
mod clipboard;
#[cfg(windows)]
mod control;
#[cfg(windows)]
mod files;
#[cfg(windows)]
mod sound;
#[cfg(windows)]
mod convert;
#[cfg(windows)]
mod cursor;
#[cfg(windows)]
mod desk;
#[cfg(windows)]
mod service;
#[cfg(windows)]
mod display;
#[cfg(windows)]
mod ccd;
#[cfg(windows)]
mod dpi;
#[cfg(windows)]
mod encode;
#[cfg(windows)]
mod d3d12enc;
#[cfg(windows)]
mod d3d12av1;
#[cfg(windows)]
mod av1probe;
#[cfg(all(windows, test))]
mod d3d12test;
#[cfg(all(windows, test))]
mod mftest;
#[cfg(all(windows, test))]
mod enctest;
#[cfg(windows)]
mod hdr;
#[cfg(windows)]
mod gamepad;
mod input;
#[cfg(windows)]
mod mf;
#[cfg(windows)]
mod mirror;
#[cfg(windows)]
mod pipeline;
#[cfg(windows)]
mod probe;
#[cfg(windows)]
mod session;
#[cfg(windows)]
mod testhdr;
#[cfg(windows)]
mod vdd;

#[cfg(not(windows))]
fn main() {
    eprintln!("remoteman-host is the Windows host; on Linux the host is remoteman-agent");
    std::process::exit(1);
}

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    windows_main::main()
}

#[cfg(windows)]
mod windows_main {
    use std::net::SocketAddr;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use anyhow::{Context, Result};
    use remoteman_core::host::{Host, HostOptions};
    use windows::Win32::Media::timeBeginPeriod;
    use windows::Win32::System::Console::SetConsoleCtrlHandler;
    use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
    use windows::core::BOOL;

    use crate::display::{Fit, Screen};
    use crate::{probe, session};

    /// Ctrl-C, the console closing, or the user signing out.
    static STOP: AtomicBool = AtomicBool::new(false);

    /// `--test-mirror-only`: no driver, and nothing done to the desktop.
    static MIRROR_ONLY: AtomicBool = AtomicBool::new(false);

    unsafe extern "system" fn on_console(_event: u32) -> BOOL {
        STOP.store(true, Ordering::Relaxed);
        true.into()
    }

    fn state_dir() -> PathBuf {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(".")).join("remoteman")
    }

    pub fn main() -> Result<()> {
        let mut listen: SocketAddr = format!("0.0.0.0:{}", remoteman_core::PORT).parse()?;
        let (mut pair, mut state, mut bitrate, mut drop_one_in) = (false, state_dir(), 40u32, None::<u32>);
        let (mut name, mut log, mut seconds, mut probing, mut fit) = (None::<String>, None::<PathBuf>, None::<u64>, None::<bool>, Fit::Any);
        let mut test_hdr = None::<u64>;
        let (mut probe_av1, mut probe_av1_encode, mut probe_size) = (false, None::<PathBuf>, (1920u32, 1088u32));
        let (mut allow_files, mut allow_clipboard) = (false, false);
        let mut ask = false;
        let mut stop_event = None::<String>;
        let mut args = std::env::args().skip(1).peekable();
        // The service (`service.rs`): it keeps a host, started with the options after the word,
        // in the console's session. Its own log goes beside the host's state.
        if args.peek().map(String::as_str) == Some("service") {
            args.next();
            let rest: Vec<String> = args.collect();
            let state = rest.iter().position(|a| a == "--state").and_then(|i| rest.get(i + 1)).map(PathBuf::from).unwrap_or_else(state_dir);
            let _ = std::fs::create_dir_all(&state);
            if let Ok(file) = std::fs::OpenOptions::new().create(true).append(true).open(state.join("service.log")) {
                tracing_subscriber::fmt().with_ansi(false).with_writer(std::sync::Mutex::new(file)).init();
            }
            return crate::service::run(rest);
        }
        // Not a host at all: a word with the one that is running.
        if args.peek().map(String::as_str) == Some("ctl") {
            args.next();
            let mut port = remoteman_core::PORT;
            if args.peek().map(String::as_str) == Some("--port") {
                args.next();
                port = args.next().context("--port needs a value")?.parse().context("--port PORT")?;
            }
            let words: Vec<String> = args.collect();
            print!("{}", crate::control::ask(port, &words.join(" "))?);
            return Ok(());
        }
        // Not a host either: this host's fingerprint, its identity made if there is none.
        if args.peek().map(String::as_str) == Some("fingerprint") {
            args.next();
            let mut state = state_dir();
            while let Some(a) = args.next() {
                match a.as_str() {
                    "--state" => state = args.next().context("--state needs a value")?.into(),
                    other => anyhow::bail!("unknown option {other}: fingerprint [--state DIR]"),
                }
            }
            println!("{}", remoteman_core::host::fingerprint(&state)?);
            return Ok(());
        }
        // For tests: announce this host on the network under NAME for SECONDS, with a made-up
        // fingerprint, and nothing else (no driver, no elevation): `--announce-test NAME SECONDS`.
        if args.peek().map(String::as_str) == Some("--announce-test") {
            args.next();
            let name = args.next().context("--announce-test NAME SECONDS")?;
            let seconds: u64 = args.next().context("--announce-test NAME SECONDS")?.parse()?;
            tracing_subscriber::fmt().with_ansi(false).init();
            let advert = crate::advertise::start(&name, remoteman_core::PORT, "0123456789abcdef0123456789abcdef");
            std::thread::sleep(Duration::from_secs(seconds));
            drop(advert);
            return Ok(());
        }
        // For looking into the display driver by hand: send it one of its text commands.
        if args.peek().map(String::as_str) == Some("--driver") {
            args.next();
            let text: Vec<String> = args.collect();
            crate::vdd::Driver::open()?.command(&text.join(" "))?;
            println!("sent");
            return Ok(());
        }
        // For tests and repairs: put the driver's first monitor into a mode for good, as the
        // main display (`--first-mode 1920x1080x60`).
        if args.peek().map(String::as_str) == Some("--first-mode") {
            args.next();
            let said = args.next().context("--first-mode WxHxR")?;
            let n: Vec<u32> = said.split('x').filter_map(|v| v.parse().ok()).collect();
            anyhow::ensure!(n.len() == 3, "--first-mode WxHxR");
            crate::display::set_first(crate::display::Mode { width: n[0], height: n[1], refresh: n[2] })?;
            println!("set");
            return Ok(());
        }
        // For tests and repairs: take an output (\\.\DISPLAY1) out of the desktop.
        if args.peek().map(String::as_str) == Some("--detach") {
            args.next();
            crate::display::detach(&args.next().context("--detach OUTPUT")?)?;
            println!("detached");
            return Ok(());
        }
        // For tests on a PC with screens of its own, without the driver: what a device's coming
        // and going does to the desktop (`--layout-test STATE beside|main|back|drop|probe OUTPUT
        // INDEX WxHxR`, see `display::layout_test`).
        if args.peek().map(String::as_str) == Some("--layout-test") {
            args.next();
            const USE: &str = "--layout-test STATE beside|main|back|drop|probe OUTPUT INDEX WxHxR, or STATE watch - SECONDS";
            let state = PathBuf::from(args.next().context(USE)?);
            let step = args.next().context(USE)?;
            let output = args.next().unwrap_or_default();
            let index: u32 = args.next().unwrap_or_else(|| "0".into()).parse().context(USE)?;
            let n: Vec<u32> = args.next().unwrap_or_else(|| "800x600x30".into()).split('x').filter_map(|v| v.parse().ok()).collect();
            anyhow::ensure!(n.len() == 3, USE);
            tracing_subscriber::fmt().with_ansi(false).init();
            std::fs::create_dir_all(&state)?;
            crate::display::layout_test(&state, &step, &output, index, crate::display::Mode { width: n[0], height: n[1], refresh: n[2] })?;
            return Ok(());
        }
        while let Some(a) = args.next() {
            let mut value = || args.next().with_context(|| format!("{a} needs a value"));
            match a.as_str() {
                "--listen" => listen = value()?.parse().context("--listen ADDR:PORT")?,
                "--state" => state = value()?.into(),
                "--bitrate" => bitrate = value()?.parse().context("--bitrate in Mbit/s")?,
                "--pair" => pair = true,
                "--ask" => ask = true,
                "--no-pair" => crate::control::NO_PAIR.store(true, Ordering::Relaxed),
                // Set by the service when it starts this host: stop when that event is set.
                "--stop-event" => stop_event = Some(value()?),
                // Which encoder: the card's own (NVENC on NVIDIA, Media Foundation's on any
                // other), or one of the two whatever the card.
                "--encoder" => crate::encode::WHICH.store(
                    match value()?.as_str() {
                        "auto" => crate::encode::AUTO,
                        "nvenc" => crate::encode::NVENC,
                        "mf" => crate::encode::ANY_MAKERS,
                        "d3d12" => crate::encode::D3D12,
                        other => anyhow::bail!("--encoder {other}: auto, nvenc, mf, d3d12"),
                    },
                    Ordering::Relaxed,
                ),
                // Static refinement: one finer frame once the picture has been still for a
                // moment (on by default; NVENC only).
                "--refine" => match value()?.as_str() {
                    "yes" | "on" => crate::pipeline::REFINE.store(true, Ordering::Relaxed),
                    "no" | "off" => crate::pipeline::REFINE.store(false, Ordering::Relaxed),
                    other => anyhow::bail!("--refine {other}: yes or no"),
                },
                // The refining frame's constant quantiser, 1 (finest) to 51; by the picture's
                // size unless given.
                "--refine-qp" => {
                    let qp: u32 = value()?.parse().context("--refine-qp: a number from 1 to 51")?;
                    anyhow::ensure!((1..=51).contains(&qp), "--refine-qp {qp}: from 1 to 51");
                    crate::pipeline::REFINE_QP.store(qp, Ordering::Relaxed);
                }
                "--allow" => {
                    for what in value()?.split(',') {
                        match what.trim() {
                            "files" => allow_files = true,
                            "clipboard" => allow_clipboard = true,
                            other => anyhow::bail!("--allow {other}: files, clipboard"),
                        }
                    }
                }
                "--name" => name = Some(value()?),
                "--log" => log = Some(value()?.into()),
                // For tests: stop by itself after this long.
                "--seconds" => seconds = Some(value()?.parse().context("--seconds N")?),
                // For tests: a window that shows HDR content on the device's monitor.
                "--probe-mic" => {}
                "--test-hdr-window" => test_hdr = Some(value()?.parse().context("--test-hdr-window SECONDS")?),
                "--probe" => probing = Some(false),
                "--probe-picture" => probing = Some(true),
                "--probe-av1" => probe_av1 = true,
                "--probe-av1-encode" => probe_av1_encode = Some(value()?.into()),
                "--probe-size" => {
                    let v = value()?;
                    let (w, h) = v.split_once('x').context("--probe-size WxH")?;
                    probe_size = (w.parse().context("--probe-size WxH")?, h.parse().context("--probe-size WxH")?);
                }
                // AV1 through Direct3D 12 (`d3d12av1.rs`), off until seen working on a card.
                "--d3d12-av1" => crate::d3d12av1::ENABLED.store(true, Ordering::Relaxed),
                "--mode" => {
                    fit = match value()?.as_str() {
                        "keep" => Fit::Keep,
                        "listed" => Fit::Listed,
                        "any" => Fit::Any,
                        other => anyhow::bail!("--mode {other}: keep, listed, any"),
                    }
                }
                // For tests, as a user who may not open the driver: serves the PC's own main screen
                // to a device that asks to mirror and nothing else; the driver isn't opened, the
                // desktop isn't arranged, and the host isn't announced on the network.
                "--test-mirror-only" => MIRROR_ONLY.store(true, Ordering::Relaxed),
                // For tests: lose one video packet in this many.
                "--test-drop-one-in" => drop_one_in = Some(value()?.parse().context("--test-drop-one-in N")?),
                other => anyhow::bail!("unknown option {other}"),
            }
        }
        if crate::control::NO_PAIR.load(Ordering::Relaxed) && (pair || ask) {
            anyhow::bail!("--no-pair can't go with --pair or --ask");
        }

        let level = if std::env::var_os("REMOTEMAN_DEBUG").is_some() { tracing::Level::DEBUG } else { tracing::Level::INFO };
        match &log {
            // To a file, for runs nobody watches (and nothing to colour there).
            Some(path) => {
                let file = std::fs::OpenOptions::new().create(true).append(true).open(path).with_context(|| format!("--log {}", path.display()))?;
                tracing_subscriber::fmt().with_max_level(level).with_ansi(false).with_writer(std::sync::Mutex::new(file)).init();
                let _ = crate::control::LOG.set(path.clone());
            }
            None => tracing_subscriber::fmt().with_max_level(level).with_ansi(false).init(),
        }
        if probe_av1 {
            return crate::av1probe::run().inspect_err(|e| tracing::error!("{e:#}"));
        }
        if let Some(dir) = probe_av1_encode {
            return crate::d3d12av1::probe(&dir, probe_size.0, probe_size.1, 240).inspect_err(|e| tracing::error!("{e:#}"));
        }
        if std::env::args().any(|a| a == "--probe-mic") {
            crate::sound::probe_formats();
            return Ok(());
        }
        if let Some(seconds) = test_hdr {
            return crate::testhdr::run(seconds).inspect_err(|e| tracing::error!("{e:#}"));
        }
        // Anything that goes wrong is in the log before the process ends.
        if let Some(event) = stop_event {
            watch_stop_event(&event)?;
        }
        let result = run(listen, state, pair, name, bitrate, drop_one_in, seconds, probing, fit, (allow_files, allow_clipboard), ask);
        if let Err(e) = &result {
            tracing::error!("{e:#}");
        }
        result
    }

    /// When the PC locks while a device is connected, the device would see nothing (Windows
    /// shows the lock screen on the PC's main screen only): it is let go with the reason
    /// `locked: `, which has its app ask for the password and come back with it.
    fn watch_lock(host: std::sync::Weak<Host>) {
        let spawned = std::thread::Builder::new().name("lock".into()).spawn(move || {
            let mut was = crate::desk::locked();
            loop {
                std::thread::sleep(Duration::from_millis(500));
                let Some(h) = host.upgrade() else { break };
                let now = crate::desk::locked();
                if now && !was && h.link().is_some() {
                    let user = crate::desk::user_name().unwrap_or_else(|| "its user".into());
                    h.close_client(&format!("locked: This computer was locked. Enter the password or PIN of {user} on it to go on."));
                    tracing::info!("the computer was locked with a device connected: the device is asked for the password");
                }
                was = now;
            }
        });
        if let Err(e) = spawned {
            tracing::warn!("couldn't start watching for the lock: {e}");
        }
    }

    /// Stop, as on Ctrl-C, when the service sets this event (`--stop-event`).
    fn watch_stop_event(name: &str) -> Result<()> {
        use windows::Win32::System::Threading::{INFINITE, OpenEventW, SYNCHRONIZATION_SYNCHRONIZE, WaitForSingleObject};
        // SAFETY: plain call; the handle is the thread's below for the life of the process.
        let event = unsafe { OpenEventW(SYNCHRONIZATION_SYNCHRONIZE, false, &windows::core::HSTRING::from(name)) }.with_context(|| format!("--stop-event {name}"))?;
        let raw = event.0 as isize;
        std::thread::Builder::new().name("stop-event".into()).spawn(move || {
            // SAFETY: the handle lives as long as the process.
            unsafe { WaitForSingleObject(windows::Win32::Foundation::HANDLE(raw as *mut std::ffi::c_void), INFINITE) };
            tracing::info!("the service asked this host to stop");
            STOP.store(true, Ordering::Relaxed);
        })?;
        Ok(())
    }

    /// Keep pairing open, and put each new device to the owner in a window of Windows' own.
    fn ask_owner(host: std::sync::Weak<Host>) {
        use windows::Win32::UI::WindowsAndMessaging::{IDYES, MB_DEFBUTTON2, MB_ICONQUESTION, MB_SETFOREGROUND, MB_TOPMOST, MB_YESNO, MessageBoxW};
        let spawned = std::thread::Builder::new().name("ask".into()).spawn(move || {
            loop {
                std::thread::sleep(Duration::from_millis(300));
                let Some(h) = host.upgrade() else { break };
                h.set_confirm(true);
                let Some(device) = h.pending() else {
                    // (Pairing closes itself after each device.)
                    if !h.pairing() {
                        h.set_pairing(true);
                    }
                    continue;
                };
                // The host isn't held while the question stands, so it can stop meanwhile.
                drop(h);
                let text = format!("\"{}\" wants to see and control this computer.\n\nIt should be showing the code {}.\n\nLet it in, now and from now on?", device.name, device.code);
                tracing::info!(device = device.name, code = device.code, "a new device: asking on the desktop");
                // SAFETY: plain call with two live strings; it returns when the owner answers.
                let answer = unsafe { MessageBoxW(None, &windows::core::HSTRING::from(text), windows::core::w!("Remoteman Host"), MB_YESNO | MB_ICONQUESTION | MB_DEFBUTTON2 | MB_TOPMOST | MB_SETFOREGROUND) };
                let Some(h) = host.upgrade() else { break };
                // (False: the minute passed before the answer, and the device was turned away.)
                let stood = h.decide(answer == IDYES);
                tracing::info!(device = device.name, let_in = answer == IDYES, in_time = stood, "the owner answered");
            }
        });
        if let Err(e) = spawned {
            tracing::warn!("couldn't start asking about new devices: {e}");
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn run(listen: SocketAddr, state: PathBuf, pair: bool, name: Option<String>, bitrate: u32, drop_one_in: Option<u32>, seconds: Option<u64>, probing: Option<bool>, fit: Fit, allow: (bool, bool), ask: bool) -> Result<()> {
        // Sizes and places in real pixels, whatever scale the desktop is drawn at: the cursor's
        // place, the monitor's corner and the pointer's target must all be in the picture's own.
        // SAFETY: plain call, before any window exists.
        if let Err(e) = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) } {
            tracing::warn!("couldn't ask Windows for real pixels ({e}); on a scaled desktop the pointer will be off");
        }
        // Sleeps of a few milliseconds (the cursor's) are that long, not a whole 15 ms tick.
        // SAFETY: plain call.
        unsafe { timeBeginPeriod(1) };
        if let Some(picture) = probing {
            return probe::run(picture);
        }

        std::fs::create_dir_all(&state).with_context(|| format!("making {}", state.display()))?;
        let mirror_only = MIRROR_ONLY.load(Ordering::Relaxed);
        let screen = if mirror_only { Screen::mirror_only() } else { Screen::open(&state)? };
        let name = name.or_else(|| std::env::var("COMPUTERNAME").ok()).unwrap_or_else(|| "windows".into());
        // What the owner switched on earlier holds until switched off; `--allow` adds to it
        // for this run.
        let allowed = crate::control::Allowed::default();
        let saved = crate::control::load(&state);
        allowed.files.store(allow.0 || saved.files, Ordering::Relaxed);
        allowed.clipboard.store(allow.1 || saved.clipboard, Ordering::Relaxed);
        allowed.paused.store(saved.paused, Ordering::Relaxed);
        allowed.lock_on_leave.store(saved.lock_on_leave, Ordering::Relaxed);
        allowed.speakers_follow.store(saved.speakers_follow, Ordering::Relaxed);
        allowed.notify.store(saved.notify, Ordering::Relaxed);
        allowed.real_screens.store(screen.pc(), Ordering::Relaxed);
        // A host that died while a device listened left the speakers switched: put them back.
        crate::speakers::init(&state);
        if saved.paused {
            tracing::info!("sharing is switched off (remembered): devices are turned away until `ctl sharing on`");
        }
        let _ = crate::control::NAME.set(name.clone());
        // A device's microphone needs a virtual sound device to be played into.
        let (sink, outputs) = crate::sound::find_microphone_sink();
        allowed.microphone.store(sink.is_some(), Ordering::Relaxed);
        match &sink {
            Some(name) => tracing::info!(into = name, "a device's microphone can be taken"),
            None => tracing::info!(?outputs, "no virtual microphone here (Steam's \"Steam Streaming Microphone\" or VB-CABLE): a device's microphone can't be taken"),
        }
        let mut screen = screen;
        let own = !mirror_only && fit != Fit::Keep && screen.has_own();
        allowed.own_monitor.store(own, Ordering::Relaxed);
        allowed.second.store(own && screen.has_second(), Ordering::Relaxed);
        // HDR for a device's own monitor: the driver's monitors take it, and NVENC says it.
        let hdr = (own || mirror_only) && crate::encode::WHICH.load(Ordering::Relaxed) != crate::encode::ANY_MAKERS && crate::encode::library().is_ok();
        allowed.hdr.store(hdr, Ordering::Relaxed);
        if mirror_only {
            tracing::info!("test host: the driver isn't used and the desktop isn't touched; a device may mirror the PC's main screen and nothing else");
        } else if !own {
            tracing::info!("the display driver has no second monitor to give (its settings say one monitor, or `--mode keep`): a device gets the one monitor at its own size");
        }
        let features = crate::control::features(&allowed);
        tracing::info!(version = env!("CARGO_PKG_VERSION"), ?features, "what this host does");
        let options = HostOptions { listen, state: state.clone(), name, os: "windows".into(), features, pair, test_drop_one_in: drop_one_in };
        let host = std::sync::Arc::new(Host::start(options, session::Host::new(screen, bitrate * 1_000_000, fit, allowed.clone()))?);
        // What the core says of devices (paired, connected, refused) is kept for the app's notifications.
        let noted = allowed.clone();
        host.set_notice_hook(move |event| crate::control::note(&noted, event));
        crate::control::serve(host.address.port(), std::sync::Arc::downgrade(&host), allowed, state);
        tracing::info!("listening on {}; this host's fingerprint is {}", host.address, host.fingerprint);
        watch_lock(std::sync::Arc::downgrade(&host));
        // On the network for viewers to find, until the host stops (dropped at the end of this function).
        let _advert = (!mirror_only).then(|| crate::advertise::start(crate::control::NAME.get().map_or("windows", String::as_str), host.address.port(), &host.fingerprint));
        if crate::control::NO_PAIR.load(Ordering::Relaxed) {
            tracing::info!("pairing: never opened by itself (--no-pair); the trust list's devices get in, and a new one only when the owner opens pairing");
        }
        if pair {
            tracing::info!("pairing: the next unknown device to connect will be trusted");
        }
        // Without the owner's app (nobody signed in, at the sign-in screen) a new device is
        // refused after 30 s instead of 60: the app asks about waiting devices every second.
        if !ask {
            host.set_approver_wait(Some(Duration::from_secs(30)));
        }
        if ask {
            tracing::info!("pairing: each new device is asked about on the desktop");
            ask_owner(std::sync::Arc::downgrade(&host));
        }

        // SAFETY: a handler that only sets a flag.
        if let Err(e) = unsafe { SetConsoleCtrlHandler(Some(on_console), true) } {
            tracing::warn!("no Ctrl-C handler ({e}); stop this process to end it");
        }
        let began = Instant::now();
        while !STOP.load(Ordering::Relaxed) && seconds.is_none_or(|s| began.elapsed() < Duration::from_secs(s)) {
            std::thread::sleep(Duration::from_millis(100));
        }
        tracing::info!("stopping");
        // Stopping the host drops the client; its handler, on a thread of the core's, then lets
        // go of the client's keys and puts the monitor back. Give it the moment that takes.
        // (The control thread holds the host only while it answers.)
        let mut host = Some(host);
        for _ in 0..20 {
            match std::sync::Arc::try_unwrap(host.take().unwrap()) {
                Ok(h) => {
                    h.stop();
                    break;
                }
                Err(still) => host = Some(still),
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        drop(host);
        std::thread::sleep(Duration::from_millis(700));
        Ok(())
    }
}
