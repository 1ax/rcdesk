use std::sync::mpsc;

use super::Options;
use crate::frame::Frame;

#[cfg(target_os = "macos")]
pub mod mac;

#[cfg(target_os = "windows")]
mod win;

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "macos")]
pub type ChannelItem = (
    screencapturekit::cm_sample_buffer::CMSampleBuffer,
    screencapturekit::sc_output_handler::SCStreamOutputType,
);
#[cfg(not(target_os = "macos"))]
pub type ChannelItem = Frame;

pub fn get_output_frame_size(options: &Options) -> [u32; 2] {
    #[cfg(target_os = "macos")]
    {
        mac::get_output_frame_size(options)
    }

    #[cfg(target_os = "windows")]
    {
        win::get_output_frame_size(options)
    }

    #[cfg(target_os = "linux")]
    {
        // TODO: How to calculate this on Linux?
        return [0, 0];
    }
}

pub struct Engine {
    options: Options,

    #[cfg(target_os = "macos")]
    mac: screencapturekit::sc_stream::SCStream,
    #[cfg(target_os = "macos")]
    error_flag: std::sync::Arc<std::sync::atomic::AtomicBool>,

    #[cfg(target_os = "windows")]
    win: win::WCStream,

    #[cfg(target_os = "linux")]
    linux: linux::LinuxCapturer,
}

impl Engine {
    pub fn new(options: &Options, tx: mpsc::Sender<ChannelItem>) -> Engine {
        #[cfg(target_os = "macos")]
        {
            let error_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let mac = mac::create_capturer(options, tx, error_flag.clone());

            Engine {
                mac,
                error_flag,
                options: (*options).clone(),
            }
        }

        #[cfg(target_os = "windows")]
        {
            let win = win::create_capturer(&options, tx);
            return Engine {
                win,
                options: (*options).clone(),
            };
        }

        #[cfg(target_os = "linux")]
        {
            let linux = linux::create_capturer(&options, tx);
            return Engine {
                linux,
                options: (*options).clone(),
            };
        }
    }

    pub fn start(&mut self) {
        #[cfg(target_os = "macos")]
        {
            // self.mac.add_output(Capturer::new(tx));
            self.mac.start_capture().expect("Failed to start capture");
        }

        #[cfg(target_os = "windows")]
        {
            self.win.start_capture();
        }

        #[cfg(target_os = "linux")]
        {
            self.linux.start_capture();
        }
    }

    pub fn stop(&mut self) {
        #[cfg(target_os = "macos")]
        {
            self.mac.stop_capture().expect("Failed to stop capture");
        }

        #[cfg(target_os = "windows")]
        {
            self.win.stop_capture();
        }

        #[cfg(target_os = "linux")]
        {
            self.linux.stop_capture();
        }
    }

    /// rcdesk patch: true if the system stopped the stream on its own
    /// (macOS `didStopWithError` sets `error_flag`). False elsewhere.
    pub fn stream_failed(&self) -> bool {
        #[cfg(target_os = "macos")]
        {
            self.error_flag.load(std::sync::atomic::Ordering::Relaxed)
        }

        #[cfg(not(target_os = "macos"))]
        {
            false
        }
    }

    /// rcdesk patch: `stop` without the `expect` on macOS, so an already
    /// stopped stream yields an error instead of a panic.
    pub fn try_stop(&mut self) -> Result<(), String> {
        #[cfg(target_os = "macos")]
        {
            self.mac.stop_capture().map_err(|e| format!("{e:?}"))
        }

        #[cfg(not(target_os = "macos"))]
        {
            self.stop();
            Ok(())
        }
    }

    pub fn get_output_frame_size(&mut self) -> [u32; 2] {
        get_output_frame_size(&self.options)
    }

    pub fn process_channel_item(&self, data: ChannelItem) -> Option<Frame> {
        #[cfg(target_os = "macos")]
        {
            mac::process_sample_buffer(data.0, data.1, self.options.output_type)
        }
        #[cfg(not(target_os = "macos"))]
        return Some(data);
    }
}
