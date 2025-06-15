/*! 
H.264 video stream parsing module.

This module provides functionality for parsing H.264 video streams in Annex B format
and integrating them with MoQ streaming protocol. It extracts SPS/PPS metadata and
handles both keyframes and regular frames.
*/

use anyhow::{Result, bail};

use bytes::BytesMut;

use futures::StreamExt;
use futures::stream::Stream;

use h264_reader::Context;
use h264_reader::annexb::AnnexBReader;
use h264_reader::nal::{pps::PicParameterSet, slice::{SliceFamily, SliceHeader}, sps::SeqParameterSet, Nal, RefNal, UnitType};
use h264_reader::push::NalInterest;

use moq_karp::{BroadcastProducer, Dimensions, H264, Frame, Timestamp, Track, TrackProducer, Video};

use std::cell::Cell;
use std::io::Read;
use std::sync::{atomic::{AtomicBool, Ordering}, Arc, Mutex, mpsc::channel};

use thiserror::Error;

/// Errors that can occur during H.264 parsing operations
#[derive(Error, Debug)]
pub enum H264ParserError {
    /// Error parsing NAL unit header
    #[error("Failed to parse NAL header: {0}")]
    NalHeaderError(String),
    /// Error parsing Sequence Parameter Set
    #[error("Failed to parse SPS: {0}")]
    SpsParseError(String),
    /// Error parsing Picture Parameter Set
    #[error("Failed to parse PPS: {0}")]
    PpsParseError(String),
    /// Error reading NAL unit data
    #[error("Failed to read NAL: {0}")]
    NalReadError(String),
    /// Error parsing slice header
    #[error("Failed to parse slice header: {0}")]
    SliceHeaderError(String),
    /// Error sending frame to MoQ track
    #[error("Failed to send frame: {0}")]
    FrameSendError(String),
    /// Error publishing MoQ track
    #[error("Failed to publish track: {0}")]
    TrackPublishError(String),
    /// Error acquiring mutex lock
    #[error("Lock error: {0}")]
    LockError(String),
}

/// Parser for H.264 video streams in Annex B format that integrates with MoQ streaming
pub struct AnnexBStreamImport {
    /// MoQ broadcast producer for sending frames
    broadcast: Arc<Mutex<BroadcastProducer>>,
    /// H.264 codec configuration extracted from the stream
    codec: Option<H264>,
    /// H.264 parsing context with SPS/PPS state
    ctx: Option<Context>,
    /// Width of the video stream in pixels
    width: u32,
    /// Height of the video stream in pixels
    height: u32,
}

impl AnnexBStreamImport {
    /// Creates a new Annex B stream parser with the specified broadcast producer and dimensions
    ///
    /// # Arguments
    ///
    /// * `broadcast` - MoQ broadcast producer for sending frames
    /// * `width` - Width of the video stream in pixels
    /// * `height` - Height of the video stream in pixels
    pub fn new(broadcast: Arc<Mutex<BroadcastProducer>>, width: u32, height: u32) -> Self {
        Self {
            broadcast,
            codec: None,
            ctx: None,
            width,
            height,
        }
    }

    /// Initializes the stream parser by extracting SPS/PPS from the input stream
    /// and creating a MoQ track
    ///
    /// # Arguments
    ///
    /// * `input` - Stream of H.264 Annex B data
    ///
    /// # Returns
    ///
    /// A TrackProducer for the video stream if initialization succeeds
    pub async fn init_from<T: Stream<Item = BytesMut> + Unpin>(&mut self, input: &mut T) -> Result<TrackProducer> {
        let mut ctx = Context::new();
        let mut sps: Option<SeqParameterSet> = None;
        let found_sps = AtomicBool::new(false);
        let found_pps = AtomicBool::new(false);

        let mut reader = AnnexBReader::accumulate(|nal: RefNal<'_>| {
            let nal_header = match nal.header() {
                Ok(header) => header,
                Err(e) => {
                    tracing::error!("Failed to parse NAL header: {:?}", e);
                    return NalInterest::Ignore;
                }
            };

            let nal_unit_type = nal_header.nal_unit_type();
            match nal_unit_type {
                UnitType::SeqParameterSet => {
                    match SeqParameterSet::from_bits(nal.rbsp_bits()) {
                        Ok(sps_local) => {
                            ctx.put_seq_param_set(sps_local.clone());
                            sps = Some(sps_local);
                            found_sps.store(true, Ordering::SeqCst);
                            NalInterest::Buffer
                        },
                        Err(e) => {
                            tracing::error!("Failed to parse SPS: {:?}", e);
                            NalInterest::Ignore
                        }
                    }
                },
                UnitType::PicParameterSet => {
                    match PicParameterSet::from_bits(&ctx, nal.rbsp_bits()) {
                        Ok(pps) => {
                            ctx.put_pic_param_set(pps);
                            found_pps.store(true, Ordering::SeqCst);
                            NalInterest::Buffer
                        },
                        Err(e) => {
                            tracing::error!("Failed to parse PPS: {:?}", e);
                            NalInterest::Ignore
                        }
                    }
                },
                _ => NalInterest::Ignore,
            }
        });

        while !found_pps.load(Ordering::SeqCst) && !found_sps.load(Ordering::SeqCst) {
            if let Some(buffer) = input.next().await {
                reader.push(&buffer);
            } else {
                break
            }
        }

        if let Some(sps) = sps {
            let codec = H264 {
                profile: sps.profile().profile_idc(),
                constraints: sps.constraint_flags.reserved_zero_two_bits(),
                level: sps.level_idc,
            };
            self.codec = Some(codec.clone());

            let track = Video {
                track: Track { name: String::from("video0"), priority: 2 },
                resolution: Dimensions {
                    width: self.width,
                    height: self.height,
                },
                codec: codec.into(),
                description: None,
                bitrate: None,
            };

            let mut broadcast_result = self.broadcast.lock()
                .map_err(|e| H264ParserError::LockError(e.to_string()))?;

            let track = broadcast_result.publish_video(track)
                .map_err(|e| H264ParserError::TrackPublishError(e.to_string()))?;

            self.ctx = Some(ctx);

            Ok(track)
        } else {
            bail!("Failed to find valid SPS in input!");
        }
    }

    /// Processes an H.264 stream, extracting frames and sending them to the MoQ track
    ///
    /// This method should be called after successful initialization with `init_from`.
    ///
    /// # Arguments
    ///
    /// * `input` - Stream of H.264 Annex B data
    /// * `track` - MoQ track to send frames to
    ///
    /// # Returns
    ///
    /// Result indicating success or error
    pub async fn read_from<T: Stream<Item = BytesMut> + Unpin>(&mut self, input: &mut T, track: &mut TrackProducer) -> Result<()> {
        if self.ctx.is_none() || self.codec.is_none() {
            bail!("AnnexBImport not initialized");
        }

        let now = std::time::Instant::now();

        let ctx = self.ctx.as_ref()
            .ok_or_else(|| anyhow::anyhow!("Context not initialized"))?;
        // NOTE: Have to use a sync channel since I want to mutate
        // both in the AnnexBReader accumulate closure and out of the
        // closure. There is probably a better way of doing this. The
        // h264_reader library has some weird API choices so it may
        // also be worth writing our own h264 NAL parser.
        let (frame_tx, frame_rx) = channel();

        let mut sps = Cell::new(BytesMut::new());
        let mut pps = Cell::new(BytesMut::new());
        let mut first_keyframe = false;

        let mut reader = AnnexBReader::accumulate(|nal: RefNal<'_>| {
            let nal_header = match nal.header() {
                Ok(header) => header,
                Err(e) => {
                    tracing::error!("Failed to parse NAL header: {:?}", e);
                    return NalInterest::Ignore;
                }
            };

            let nal_unit_type = nal_header.nal_unit_type();
            match nal_unit_type {
                UnitType::PicParameterSet => {
                    if nal.is_complete() {
                        let mut nal_reader = nal.reader();
                        let mut full_nal_buf = BytesMut::new();
                        let mut buf = [0u8; 1024];
                        loop {
                            let n = match nal_reader.read(&mut buf) {
                                Ok(n) => n,
                                Err(e) => {
                                    tracing::error!("Failed to read NAL: {}", e);
                                    break;
                                }
                            };
                            if n == 0 {
                                break
                            } else {
                                full_nal_buf.extend_from_slice(&buf);
                            }
                        }
                        pps.set(full_nal_buf);
                    }
                    NalInterest::Buffer
                }
                UnitType::SeqParameterSet => {
                    if nal.is_complete() {
                        let mut nal_reader = nal.reader();
                        let mut full_nal_buf = BytesMut::new();
                        let mut buf = [0u8; 1024];
                        loop {
                            let n = match nal_reader.read(&mut buf) {
                                Ok(n) => n,
                                Err(e) => {
                                    tracing::error!("Failed to read NAL: {}", e);
                                    break;
                                }
                            };
                            if n == 0 {
                                break
                            } else {
                                full_nal_buf.extend_from_slice(&buf);
                            }
                        }

                        sps.set(full_nal_buf);
                    }
                    NalInterest::Buffer
                },
                UnitType::SliceLayerWithoutPartitioningNonIdr => {
                    if nal.is_complete() && first_keyframe {
                        let ts = now.elapsed().as_micros();

                        let nal_header = match nal.header() {
                            Ok(header) => header,
                            Err(e) => {
                                tracing::error!("Failed to parse NAL header: {:?}", e);
                                return NalInterest::Ignore;
                            }
                        };

                        let slice_header = match SliceHeader::from_bits(ctx, &mut nal.rbsp_bits(), nal_header) {
                            Ok((header, _, _)) => header,
                            Err(e) => {
                                tracing::error!("Failed to parse slice header: {:?}", e);
                                return NalInterest::Ignore;
                            }
                        };
                        let keyframe = slice_header.slice_type.family == SliceFamily::I;

                        let mut nal_reader = nal.reader();
                        let mut full_nal_buf = BytesMut::new();
                        let mut buf = [0u8; 1024];
                        loop {
                            let n = match nal_reader.read(&mut buf) {
                                Ok(n) => n,
                                Err(e) => {
                                    tracing::error!("Failed to read NAL: {}", e);
                                    break;
                                }
                            };
                            if n == 0 {
                                break
                            } else {
                                full_nal_buf.extend_from_slice(&buf);
                            }
                        }

                        if !keyframe {
                            let mut payload = BytesMut::new();
                            payload.extend_from_slice(&[0u8, 0u8, 0u8, 1u8]);
                            payload.extend_from_slice(&full_nal_buf);
                            let frame = Frame {
                                timestamp: Timestamp::from_micros(ts as u64),
                                keyframe,
                                payload: payload.freeze(),
                            };
                            if let Err(e) = frame_tx.send(frame) {
                                tracing::error!("Failed to send frame: {}", e);
                            };
                        } else if keyframe && !pps.get_mut().is_empty() && !sps.get_mut().is_empty() {
                            let sps = sps.get_mut();
                            let pps = pps.get_mut();
                            let mut payload = BytesMut::new();
                            payload.extend_from_slice(&[0u8, 0u8, 0u8, 1u8]);
                            payload.extend_from_slice(sps);
                            payload.extend_from_slice(&[0u8, 0u8, 0u8, 1u8]);
                            payload.extend_from_slice(pps);
                            payload.extend_from_slice(&[0u8, 0u8, 0u8, 1u8]);
                            payload.extend_from_slice(&full_nal_buf);
                            let frame = Frame {
                                timestamp: Timestamp::from_micros(ts as u64),
                                keyframe,
                                payload: payload.freeze(),
                            };
                            if let Err(e) = frame_tx.send(frame) {
                                tracing::error!("Failed to send frame: {}", e);
                            };
                        }
                    }
                    NalInterest::Buffer
                },
                UnitType::SliceLayerWithoutPartitioningIdr => {
                    if nal.is_complete() && !pps.get_mut().is_empty() && !sps.get_mut().is_empty() {
                        if !first_keyframe {
                            first_keyframe = true;
                        }
                        let ts = now.elapsed().as_micros();
                        let mut nal_reader = nal.reader();
                        let mut full_nal_buf = BytesMut::new();
                        let mut buf = [0u8; 1024];
                        loop {
                            let n = match nal_reader.read(&mut buf) {
                                Ok(n) => n,
                                Err(e) => {
                                    tracing::error!("Failed to read NAL: {}", e);
                                    break;
                                }
                            };
                            if n == 0 {
                                break
                            } else {
                                full_nal_buf.extend_from_slice(&buf);
                            }
                        }

                        let sps = sps.get_mut();
                        let pps = pps.get_mut();
                        let mut payload = BytesMut::new();
                        payload.extend_from_slice(&[0u8, 0u8, 0u8, 1u8]);
                        payload.extend_from_slice(sps);
                        payload.extend_from_slice(&[0u8, 0u8, 0u8, 1u8]);
                        payload.extend_from_slice(pps);
                        payload.extend_from_slice(&[0u8, 0u8, 0u8, 1u8]);
                        payload.extend_from_slice(&full_nal_buf);

                        let frame = Frame {
                            timestamp: Timestamp::from_micros(ts as u64),
                            keyframe: true,
                            payload: payload.freeze(),
                        };
                        if let Err(e) = frame_tx.send(frame) {
                                tracing::error!("Failed to send frame: {}", e);
                            };
                    }
                    NalInterest::Buffer
                },
                _ => {
                    NalInterest::Ignore
                },
            }
        });
        
        while let Some(buffer) = input.next().await {
            reader.push(&buffer);
            loop {
                match frame_rx.try_recv() {
                    Ok(f) => {
                        track.write(f);
                    },
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(_) => panic!("frame_rx channel disconnected"),
                }
            }
        }

        Ok(())
    }
}
