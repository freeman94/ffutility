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

#[derive(Error, Debug)]
pub enum V4lStreamError {
    #[error("Failed to open V4L device: {0}")]
    DeviceOpenError(String),
    #[error("Failed to get V4L formats: {0}")]
    DeviceFormatError(String),
    #[error("Failed to create MmapStream: {0}")]
    MmapStreamError(String),
    #[error("Failed to get V4L format: {0}")]
    GetFormatError(String),
    #[error("Failed to create H264 encoder: {0}")]
    EncoderCreationError(#[from] crate::encoders::H264EncoderError),
    // Renamed to avoid conflict with EncoderCreationError
    #[error("Error encoding frame: {0}")]
    FrameEncodingError(crate::encoders::H264EncoderError),
    #[error("Stream iteration error: {0}")]
    StreamIterationError(String),
    #[error("Channel send error: {0}")]
    ChannelSendError(#[from] tokio::sync::mpsc::error::SendError<BytesMut>),
}

// TODO: make this more generic so you can have a v4l stream with
// different encoder types (e.g AV1)

pub struct V4lH264Config {
    pub output_width: u32,
    pub output_height: u32,
    pub bitrate: usize,
    pub input_type: InputType,
    pub v4l_fourcc: v4l::FourCC,
    pub video_dev: String,
}

pub struct V4lH264Stream {
}

impl V4lH264Stream {
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

// Helper function to run the V4L stream with proper error handling
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
