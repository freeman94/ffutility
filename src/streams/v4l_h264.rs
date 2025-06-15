/*! 
V4L2 H.264 video streaming module.

This module provides functionality for capturing video from V4L2 devices
and encoding it to H.264 format. It supports various input formats and
configurable output parameters.
*/

use anyhow::Result;

use bytes::BytesMut;

use crate::encoders::{EncoderConfig, EncoderType, FfmpegOptions, H264Encoder, InputType};

use tokio::sync::mpsc;

use tokio_stream::wrappers::ReceiverStream;

use thiserror::Error;

use tracing::{debug, error};

use v4l::buffer::Type;
use v4l::io::traits::CaptureStream;
use v4l::video::traits::Capture;
use v4l::prelude::*;

/// Errors that can occur during V4L2 video streaming operations
#[derive(Error, Debug)]
pub enum V4lStreamError {
    /// Failed to open the V4L2 device
    #[error("Failed to open V4L device: {0}")]
    DeviceOpenError(String),
    /// Failed to enumerate or set V4L2 device formats
    #[error("Failed to get V4L formats: {0}")]
    DeviceFormatError(String),
    /// Failed to create memory-mapped stream from the V4L2 device
    #[error("Failed to create MmapStream: {0}")]
    MmapStreamError(String),
    /// Failed to get the current format from the V4L2 device
    #[error("Failed to get V4L format: {0}")]
    GetFormatError(String),
    /// Failed to create the H.264 encoder
    #[error("Failed to create H264 encoder: {0}")]
    EncoderCreationError(#[from] crate::encoders::H264EncoderError),
    /// Error during frame encoding
    #[error("Error encoding frame: {0}")]
    FrameEncodingError(crate::encoders::H264EncoderError),
    /// Error iterating through the V4L2 stream
    #[error("Stream iteration error: {0}")]
    StreamIterationError(String),
    /// Error sending encoded frame through the channel
    #[error("Channel send error: {0}")]
    ChannelSendError(#[from] tokio::sync::mpsc::error::SendError<BytesMut>),
}

// TODO: make this more generic so you can have a v4l stream with
// different encoder types (e.g AV1)

/// Configuration for V4L2 H.264 video stream
pub struct V4lH264Config {
    /// Width of the output encoded video in pixels
    pub output_width: u32,
    /// Height of the output encoded video in pixels
    pub output_height: u32,
    /// Target bitrate for H.264 encoding in bits per second
    pub bitrate: usize,
    /// Pixel format for the input frames
    pub input_type: InputType,
    /// FourCC code representing the V4L2 device's pixel format
    pub v4l_fourcc: v4l::FourCC,
    /// Path to the V4L2 device (e.g., "/dev/video0")
    pub video_dev: String,
}

/// Stream handler for V4L2 H.264 video capture and encoding
pub struct V4lH264Stream {
}

impl V4lH264Stream {
    /// Creates a new V4L2 H.264 stream and returns a receiver for encoded frames
    ///
    /// This function spawns a separate thread that handles device I/O and encoding,
    /// returning a stream of encoded H.264 frames.
    ///
    /// # Arguments
    ///
    /// * `cfg` - Configuration for the V4L2 stream and encoder
    /// * `ffmpeg_opts` - Additional FFmpeg options for the H.264 encoder
    ///
    /// # Returns
    ///
    /// A stream of encoded H.264 NAL units as BytesMut
    pub fn new(cfg: V4lH264Config, ffmpeg_opts: FfmpegOptions) -> Result<ReceiverStream<BytesMut>> {
        let (tx, rx) = mpsc::channel::<BytesMut>(10);

        std::thread::spawn(move || {
            // Properly handle errors and close the channel when done
            if let Err(err) = run_v4l_stream(&cfg, &ffmpeg_opts, tx.clone()) {
                error!("V4L stream error: {}", err);
            }
            // Close the channel by dropping tx
            drop(tx);
        });

        Ok(ReceiverStream::from(rx))
    }
}

/// Helper function to manage V4L2 device connection and frame encoding
///
/// This function handles:
/// - Waiting for the V4L2 device to be available
/// - Configuring the device and encoder
/// - Capturing frames, encoding them, and sending them through the channel
///
/// # Arguments
///
/// * `cfg` - Configuration for the V4L2 stream
/// * `ffmpeg_opts` - Additional FFmpeg options for the encoder
/// * `tx` - Channel sender for encoded frames
///
/// # Returns
///
/// Result indicating success or detailed error information
fn run_v4l_stream(
    cfg: &V4lH264Config,
    ffmpeg_opts: &FfmpegOptions,
    tx: mpsc::Sender<BytesMut>,
) -> Result<(), V4lStreamError> {
    loop {
        // Block until the v4l_device is up
        let v4l_dev = Device::with_path(&cfg.video_dev)
            .map_err(|e| V4lStreamError::DeviceOpenError(e.to_string()))?;

        let formats = v4l_dev.enum_formats()
            .map_err(|e| V4lStreamError::DeviceFormatError(e.to_string()))?;

        tracing::trace!("{} got formats: {:?}", &cfg.video_dev.as_str(), formats);

        if !formats.iter().any(|fmt| fmt.fourcc == cfg.v4l_fourcc) {
            tracing::error!("{} doesn't have correct FourCC!", &cfg.video_dev.as_str());
            std::thread::sleep(std::time::Duration::from_secs(1));
        } else {
            tracing::info!("{} has correct FourCC!", &cfg.video_dev.as_str());
            break;
        }
    }

    let video_dev = Device::with_path(&cfg.video_dev)
        .map_err(|e| V4lStreamError::DeviceOpenError(e.to_string()))?;

    let mut stream = MmapStream::new(&video_dev, Type::VideoCapture)
        .map_err(|e| V4lStreamError::MmapStreamError(e.to_string()))?;

    let format = video_dev.format()
        .map_err(|e| V4lStreamError::GetFormatError(e.to_string()))?;

    debug!("V4L Format: {:?}", format);
    let ec = EncoderConfig {
        input_width: format.width,
        input_height: format.height,
        output_width: cfg.output_width,
        output_height: cfg.output_height,
        framerate: 15,
        gop: None,
        bitrate: cfg.bitrate,
        disable_b_frames: false,
        enc_type: EncoderType::X264,
        input_type: cfg.input_type,
    };

    let mut pts = 0;
    let mut encoder = H264Encoder::new(ec, ffmpeg_opts)?;

    loop {
        let (m_buf, meta) = stream.next()
            .map_err(|e| V4lStreamError::StreamIterationError(e.to_string()))?;

        let bytesused = meta.bytesused as usize;

        if let Some(encoded_frame) = encoder.encode_raw(Some(pts), &m_buf[..bytesused]).map_err(V4lStreamError::FrameEncodingError)? {
            tx.blocking_send(encoded_frame.nal_bytes)?;
        }
        pts += 1;
    }
}
