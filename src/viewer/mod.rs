//! The viewer: a window that shows the device's screen and sends it touch and keys, like scrcpy's.
//!
//! `scrios` without a subcommand opens it (`scrios stream` stays headless). The pieces:
//!
//! * [`decode`]: H.264 access units to pictures: `VideoToolbox` on macOS, `OpenH264` in software anywhere;
//! * [`render`]: wgpu draws the picture, turned upright and letterboxed, from its two planes (NV12);
//! * [`geometry`] and [`input`]: where the picture is in the window and what a click or a key on it means for the
//!   device (touch id 0, HID keys, the shortcut keys);
//! * [`pipeline`]: the decode and input threads and the one-picture slot between the decoder and the screen;
//! * [`stats`]: the window title and the `--show-stats` numbers;
//! * `app`: winit's event loop that ties them together.
//!
//! [`Setup::prepare`] checks everything that can be checked before a device is touched (the decoder, the recording
//! file); [`Setup::run`] then takes the window's thread and the [`Feed`] (a [`Stream`](crate::stream::Stream)), and
//! returns the exit status when the window is closed or the stream ends.

use std::fs::File;
use std::path::PathBuf;
use std::sync::Arc;

use tracing::error;
use winit::event_loop::EventLoop;

use crate::codec::CodecKind;
use crate::stream::EXIT_START_FAILED;

mod app;
pub mod decode;
pub mod geometry;
pub mod input;
pub mod pipeline;
pub mod render;
pub mod stats;

pub use decode::DecoderChoice;
pub use pipeline::{Feed, Recorder};

use pipeline::{Control, FrameSlot, Wake, decode_loop};

/// What the viewer is asked for.
#[derive(Debug, Clone)]
pub struct ViewerOptions {
    /// The codec the stream is asked for: the viewer decodes H.264 only.
    pub codec: CodecKind,
    pub decoder: DecoderChoice,
    /// Send touch and keys to the device (off: view only).
    pub control: bool,
    pub title: String,
    /// The largest the first window may be, in logical pixels; the screen otherwise.
    pub window_width: Option<u32>,
    pub window_height: Option<u32>,
    /// Put the frame rate and the viewer's latency in the window title.
    pub show_stats: bool,
    /// `MOD+P` locks the device (off by default: locking from the keyboard is a way to lock oneself out).
    pub lock_shortcut: bool,
    /// Record the stream, as it comes, to this file (raw Annex-B).
    pub record: Option<PathBuf>,
}

impl Default for ViewerOptions {
    fn default() -> Self {
        ViewerOptions {
            codec: CodecKind::H264,
            decoder: DecoderChoice::Auto,
            control: true,
            title: "scrios".to_owned(),
            window_width: None,
            window_height: None,
            show_stats: false,
            lock_shortcut: false,
            record: None,
        }
    }
}

/// A viewer that is ready to run: its decoder exists and its recording file is open.
pub struct Setup {
    options: ViewerOptions,
    decoder: Box<dyn decode::Decoder>,
    recorder: Option<Recorder>,
}

impl std::fmt::Debug for Setup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Setup")
            .field("options", &self.options)
            .field("decoder", &self.decoder.name())
            .field("recording", &self.recorder.is_some())
            .finish()
    }
}

impl Setup {
    /// Makes the decoder and opens the recording, before anything is started on the device.
    ///
    /// # Errors
    ///
    /// The codec is not H.264, the decoder asked for is not available, or the recording cannot be written. The message
    /// says which.
    pub fn prepare(options: ViewerOptions) -> Result<Setup, String> {
        let decoder = decode::create(options.decoder, options.codec)?;
        let recorder = match &options.record {
            Some(path) => {
                let file = File::create(path).map_err(|e| format!("cannot write --record {}: {e}", path.display()))?;
                Some(Recorder::new(path.display().to_string(), file))
            }
            None => None,
        };
        Ok(Setup { options, decoder, recorder })
    }

    /// The name of the decoder in use (for the log).
    #[must_use]
    pub fn decoder_name(&self) -> &'static str {
        self.decoder.name()
    }

    /// Opens the window on this thread, which must be the program's main thread (macOS needs that), and runs it until it
    /// is closed or the stream ends. Stops the stream on the way out. Returns the exit status.
    pub fn run(self, feed: Arc<dyn Feed>) -> i32 {
        let Setup { options, mut decoder, recorder } = self;
        let events = match EventLoop::<Wake>::with_user_event().build() {
            Ok(events) => events,
            Err(e) => {
                error!("cannot start the window system: {e}");
                feed.hang_up();
                return EXIT_START_FAILED;
            }
        };
        let proxy = events.create_proxy();
        let slot = Arc::new(FrameSlot::default());
        let decoder_name = decoder.name();

        let spawned = std::thread::Builder::new().name("scrios-viewer-decode".into()).spawn({
            let (feed, slot) = (feed.clone(), slot.clone());
            move || {
                let wake = move |w: Wake| {
                    // The window is gone when this fails: nobody to tell.
                    let _ = proxy.send_event(w);
                };
                decode_loop(feed.as_ref(), decoder.as_mut(), &slot, recorder, &wake);
            }
        });
        if let Err(e) = spawned {
            error!("cannot start the decoder thread: {e}");
            feed.hang_up();
            return EXIT_START_FAILED;
        }

        let control = if options.control {
            match Control::start(feed.clone()) {
                Ok(control) => control,
                Err(e) => {
                    error!("cannot start the input thread: {e}");
                    feed.hang_up();
                    return EXIT_START_FAILED;
                }
            }
        } else {
            Control::off()
        };
        let window = app::WindowOptions {
            title: options.title,
            width: options.window_width,
            height: options.window_height,
            show_stats: options.show_stats,
            control: options.control,
            lock_shortcut: options.lock_shortcut,
            decoder: decoder_name,
        };
        app::run(window, feed, slot, control, events)
    }
}
