/*! 
H.264 video encoding module.

This module provides H.264 encoding functionality with support for:
- Software encoding using libx264
- Hardware-accelerated encoding using NVIDIA NVENC
- Jetson-specific encoding using NVMPI
*/

use ffmpeg::codec::codec::Codec as AvCodec;
use ffmpeg::codec::context::Context as AvContext;
use ffmpeg::codec::encoder::video::Encoder as AvVideoEncoder;
use ffmpeg::codec::packet::Packet as AvPacket;
use ffmpeg::software::scaling::{context::Context as AvScalingContext, flag::Flags};
use ffmpeg::util::format::Pixel as AvPixel;
use ffmpeg::util::frame::Video as AvFrame;
use ffmpeg::util::rational::Rational;
use ffmpeg::Dictionary as AvDictionary;
use ffmpeg::Error as AvError;
use ffmpeg_next as ffmpeg;

#[cfg(feature = "opencv")]
use opencv::{
    core::Mat,
    prelude::{MatTraitConst, MatTraitConstManual},
};

use serde::{Deserialize, Serialize};

use thiserror::Error;

use tracing::{debug, error};

/// Options to configure the FFmpeg encoder, provided as key-value pairs
pub type FfmpegOptions = Vec<(String, String)>;
/// Input pixel format for the encoder, using FFmpeg's pixel format enumeration
pub type InputType = AvPixel;

/// Supported H.264 encoder types
#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum EncoderType {
    /// Software H.264 encoder (libx264)
    #[serde(rename = "libx264")]
    X264,
    /// NVIDIA GPU hardware-accelerated H.264 encoder
    #[serde(rename = "h264_nvenc")]
    H264Nvenc,
    /// Jetson-specific NVIDIA hardware-accelerated H.264 encoder
    /// 
    /// Note: Only available on Jetson platforms with NVMPI support
    #[serde(rename = "h264_nvmpi")]
    H264Nvmpi,
}

impl EncoderType {
    /// Converts the encoder type to its FFmpeg codec name string
    fn as_str(&self) -> &'static str {
        match self {
            Self::X264 => "libx264",
            Self::H264Nvenc => "h264_nvenc",
            Self::H264Nvmpi => "h264_nvmpi",
        }
    }
}

/// Configuration options for the H.264 encoder
#[derive(Clone, Debug)]
pub struct EncoderConfig {
    /// Width of the input frames in pixels
    pub input_width: u32,
    /// Height of the input frames in pixels
    pub input_height: u32,
    /// Width of the encoded output in pixels
    pub output_width: u32,
    /// Height of the encoded output in pixels
    pub output_height: u32,
    /// Frame rate in frames per second
    pub framerate: u32,
    /// Group of pictures size (I-frame interval)
    pub gop: Option<u32>,
    /// Target bitrate in bits per second
    pub bitrate: usize,
    /// Whether to disable B-frames in the encoding
    pub disable_b_frames: bool,
    /// Type of encoder to use (software or hardware)
    pub enc_type: EncoderType,
    /// Input pixel format
    pub input_type: InputType,
}

/// Errors that can occur during H.264 encoding operations
#[derive(Error, Debug)]
pub enum H264EncoderError {
    /// Presentation timestamps must be monotonically increasing
    #[error("PTS was not monotonically increasing: previous PTS: {prev_pts} > PTS: {curr_pts}")]
    PTSNotMonotonic {
        prev_pts: i64,
        curr_pts: i64
    },
    /// FFmpeg library errors
    #[error("ffmpeg error: {0}")]
    FfmpegError(#[from] AvError),
    /// Failed to allocate FFmpeg codec context
    #[error("failed to alloc avcodec context")]
    AvCodecAllocContextError,
    /// The requested encoder codec was not found
    #[error("codec not found: {0}")]
    CodecNotFound(String),
    /// Failed to create video encoder
    #[error("failed to create video encoder")]
    EncoderCreationFailed,
    /// Error during frame scaling operations
    #[error("scaling error: {0}")]
    ScalingError(String),
    /// Error during frame encoding process
    #[error("frame encoding error: {0}")]
    FrameEncodeError(String),
    /// Error reading frame data
    #[error("frame data read error: {0}")]
    FrameDataReadError(String),
}

/// H.264 video encoder that uses FFmpeg to encode frames
pub struct H264Encoder {
    /// FFmpeg video encoder instance
    encoder: AvVideoEncoder,
    /// FFmpeg scaling context for format conversion
    scaler: AvScalingContext,
    /// Previous presentation timestamp for monotonicity check
    prev_pts: Option<i64>,
    /// Input pixel format
    input_type: AvPixel,
    /// Width of input frames in pixels
    input_width: u32,
    /// Height of input frames in pixels
    input_height: u32,
    /// Width of encoded output in pixels
    output_width: u32,
    /// Height of encoded output in pixels
    output_height: u32,
}

/// Creates an FFmpeg codec context from a codec
/// 
/// # Safety
/// 
/// This function uses unsafe FFmpeg API calls to allocate a codec context
fn codec_context_as(codec: &AvCodec) -> Result<AvContext, H264EncoderError> {
    unsafe {
        let context_ptr = ffmpeg::ffi::avcodec_alloc_context3(codec.as_ptr());
        if !context_ptr.is_null() {
            Ok(AvContext::wrap(context_ptr, None))
        } else {
            Err(H264EncoderError::AvCodecAllocContextError)
        }
    }
}

/// Represents an encoded H.264 frame with metadata
pub struct EncodedFrame {
    /// The encoded NAL unit bytes
    pub nal_bytes: bytes::BytesMut,
    /// Whether this frame is a keyframe (I-frame)
    pub is_keyframe: bool,
    /// Duration of the frame in the encoder's timebase
    pub duration: i64,
    /// Presentation timestamp of the frame
    pub pts: Option<i64>,
}

unsafe impl Send for H264Encoder {}
unsafe impl Sync for H264Encoder {}

impl H264Encoder {
    /// Creates a new H.264 encoder with the specified configuration
    ///
    /// # Arguments
    ///
    /// * `ec` - Encoder configuration options
    /// * `extra_ffmpeg_opts` - Additional FFmpeg-specific encoder options as key-value pairs
    ///
    /// # Returns
    ///
    /// A configured H.264 encoder ready to encode frames
    pub fn new(
        ec: EncoderConfig,
        extra_ffmpeg_opts: &FfmpegOptions,
    ) -> Result<Self, H264EncoderError> {
        let mut extra_opts = AvDictionary::new();
        for opt in extra_ffmpeg_opts {
            extra_opts.set(opt.0.as_str(), opt.1.as_str());
        }

        let ffmpeg_codec = ffmpeg::encoder::find_by_name(ec.enc_type.as_str())
            .ok_or(H264EncoderError::CodecNotFound(ec.enc_type.as_str().to_string()))?;

        let ffmpeg_context = codec_context_as(&ffmpeg_codec)?;
        let ffmpeg_video = ffmpeg_context.encoder().video();
        let mut ffmpeg_vid_encoder = if let Ok(encoder) = ffmpeg_video {
            encoder
        } else {
            return Err(H264EncoderError::EncoderCreationFailed);
        };

        ffmpeg_vid_encoder.set_width(ec.output_width);
        ffmpeg_vid_encoder.set_height(ec.output_height);
        ffmpeg_vid_encoder.set_format(AvPixel::YUV420P);
        ffmpeg_vid_encoder.set_frame_rate(Some((ec.framerate as i32, 1)));
        ffmpeg_vid_encoder.set_time_base(Rational(1, ec.framerate as i32));
        ffmpeg_vid_encoder.set_bit_rate(ec.bitrate);
        if ec.disable_b_frames {
            ffmpeg_vid_encoder.set_max_b_frames(0_usize);
        }
        if let Some(gop) = ec.gop {
            ffmpeg_vid_encoder.set_gop(gop);
        }
        let encoder = ffmpeg_vid_encoder.open_with(extra_opts)?;

        let scaler = AvScalingContext::get(
            ec.input_type,
            ec.input_width,
            ec.input_height,
            AvPixel::YUV420P,
            ec.output_width,
            ec.output_height,
            Flags::BILINEAR,
        )?;

        Ok(Self {
            encoder,
            scaler,
            prev_pts: None,
            input_type: ec.input_type,
            input_width: ec.input_width,
            input_height: ec.input_height,
            output_width: ec.output_width,
            output_height: ec.output_height,
        })
    }

    /// Encodes an OpenCV Mat frame to H.264
    ///
    /// This method is only available when the `opencv` feature is enabled.
    ///
    /// # Arguments
    ///
    /// * `pts` - Optional presentation timestamp for the frame
    /// * `input` - OpenCV Mat containing the frame data
    ///
    /// # Returns
    ///
    /// An encoded H.264 frame if encoding was successful
    #[cfg(feature = "opencv")]
    pub fn encode_mat(&mut self, pts: Option<i64>, input: &Mat) -> Result<Option<EncodedFrame>, H264EncoderError> {
        let width = input.cols();
        let height = input.rows();

        let width_u32 = width.try_into().map_err(|_| H264EncoderError::FrameEncodeError("Invalid width".to_string()))?;
        let height_u32 = height.try_into().map_err(|_| H264EncoderError::FrameEncodeError("Invalid height".to_string()))?;

        let mut out_frame = AvFrame::new(
            AvPixel::YUV420P,
            width_u32,
            height_u32,
        );

        let mut in_frame = AvFrame::new(
            self.input_type,
            width_u32,
            height_u32,
        );

        let data_bytes = input.data_bytes()
            .map_err(|e| H264EncoderError::FrameDataReadError(e.to_string()))?;

        in_frame
            .data_mut(0)
            .copy_from_slice(data_bytes);

        self.scaler.run(&in_frame, &mut out_frame)
            .map_err(|e| H264EncoderError::ScalingError(e.to_string()))?;

        self.encode(pts, out_frame)
    }

    /// Encodes raw byte data to H.264
    ///
    /// # Arguments
    ///
    /// * `pts` - Optional presentation timestamp for the frame
    /// * `input` - Raw byte data containing the frame
    ///
    /// # Returns
    ///
    /// An encoded H.264 frame if encoding was successful
    pub fn encode_raw(&mut self, pts: Option<i64>, input: &[u8]) -> Result<Option<EncodedFrame>, H264EncoderError> {
        debug!("input len: {}", input.len());

        let out_width: i32 = self.output_width.try_into()
            .map_err(|_| H264EncoderError::FrameEncodeError("Invalid output width".to_string()))?;

        let out_height: i32 = self.output_height.try_into()
            .map_err(|_| H264EncoderError::FrameEncodeError("Invalid output height".to_string()))?;

        let in_width: i32 = self.input_width.try_into()
            .map_err(|_| H264EncoderError::FrameEncodeError("Invalid input width".to_string()))?;

        let in_height: i32 = self.input_height.try_into()
            .map_err(|_| H264EncoderError::FrameEncodeError("Invalid input height".to_string()))?;

        let mut out_frame = AvFrame::new(
            AvPixel::YUV420P,
            out_width.try_into().map_err(|_| H264EncoderError::FrameEncodeError("Invalid output width conversion".to_string()))?,
            out_height.try_into().map_err(|_| H264EncoderError::FrameEncodeError("Invalid output height conversion".to_string()))?,
        );

        let mut in_frame = AvFrame::new(
            self.input_type,
            in_width.try_into().map_err(|_| H264EncoderError::FrameEncodeError("Invalid input width conversion".to_string()))?,
            in_height.try_into().map_err(|_| H264EncoderError::FrameEncodeError("Invalid input height conversion".to_string()))?,
        );

        if in_frame.planes() > 1 {
            let mut start_id = 0;
            for i in 0..in_frame.planes() {
                let buf_size = in_frame.data(i).len();
                let end_id = start_id + buf_size;

                if end_id > input.len() {
                    return Err(H264EncoderError::FrameEncodeError(format!(
                        "Input buffer too small: {} < {}", input.len(), end_id
                    )));
                }

                in_frame.data_mut(i).copy_from_slice(&input[start_id..end_id]);
                start_id = end_id;
            }
        } else {
            if input.len() < in_frame.data(0).len() {
                return Err(H264EncoderError::FrameEncodeError(format!(
                    "Input buffer too small: {} < {}", input.len(), in_frame.data(0).len()
                )));
            }

            in_frame.data_mut(0).copy_from_slice(input);
        }

        self.scaler.run(&in_frame, &mut out_frame)
            .map_err(|e| H264EncoderError::ScalingError(e.to_string()))?;

        self.encode(pts, out_frame)
    }

    /// Internal method to encode an FFmpeg frame to H.264
    ///
    /// # Arguments
    ///
    /// * `pts` - Optional presentation timestamp for the frame
    /// * `frame` - FFmpeg video frame to encode
    ///
    /// # Returns
    ///
    /// An encoded H.264 frame if encoding was successful
    pub fn encode(&mut self, pts: Option<i64>, mut frame: AvFrame) -> Result<Option<EncodedFrame>, H264EncoderError> {
        if let Some(prev_pts) = self.prev_pts {
            if let Some(curr_pts) = pts {
                if prev_pts > curr_pts {
                    return Err(H264EncoderError::PTSNotMonotonic {
                        prev_pts,
                        curr_pts
                    })
                }
            }
        }

        frame.set_pts(pts);
        self.prev_pts = pts;

        self.encoder.send_frame(&frame)
            .map_err(|e| H264EncoderError::FrameEncodeError(e.to_string()))?;

        self.retrieve_nal()
    }

    /// Drains and consumes this encoder
    pub fn drain(mut self) -> Result<Vec<EncodedFrame>, H264EncoderError> {
        self.encoder.send_eof()?;
        let mut result = vec![];
        while let Some(nal) = self.retrieve_nal()? {
            result.push(nal);
        }
        Ok(result)
    }

    /// Drain a NAL out of the encoder. Typically not used directly,
    /// except after `begin_drain`.
    fn retrieve_nal(&mut self) -> Result<Option<EncodedFrame>, H264EncoderError> {
        let mut encoded_packet = AvPacket::empty();
        let encoder_res = self.encoder.receive_packet(&mut encoded_packet);

        match encoder_res {
            Ok(_) => {
                let encoded_data = encoded_packet.data();
                if encoded_data.is_none() {
                    error!("encoder likely dropping frames!! data packet is empty");
                    Ok(None)
                } else {
                    // We've already checked encoded_data is Some, so this is just to satisfy
                    // the borrow checker and maintain the same logic structure
                    encoded_data.map(|nal| {
                        EncodedFrame {
                            nal_bytes: bytes::BytesMut::from(nal),
                            is_keyframe: encoded_packet.is_key(),
                            duration: encoded_packet.duration(),
                            pts: encoded_packet.pts(),
                        }
                    }).map(Some).ok_or_else(|| {
                        // This shouldn't happen as we've already checked for None
                        H264EncoderError::FrameEncodeError("Empty encoded data packet".to_string())
                    })
                }
            }
            Err(e) if e.to_string().contains("encoder buffer full") => {
                // This is not an error, just means we need to send more frames
                Ok(None)
            }
            Err(e) if e.to_string().contains("encoder flushed") => {
                // Also normal during flushing
                Ok(None)
            }
            Err(e) => {
                debug!("got ffmpeg encoder error: {e}");
                // Only return an error for significant issues
                if e.to_string().contains("again") {
                    // "Try again" type errors are common and expected
                    Ok(None)
                } else {
                    Err(H264EncoderError::FfmpegError(e))
                }
            }
        }
    }
}
