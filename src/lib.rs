/*! 
# FFUtility

A grab bag of video streaming utilities for working with various video formats, encoders, and streaming protocols.

This library provides tools for:
- Parsing H.264 video streams
- Encoding video to H.264
- Capturing video from V4L2 devices
- Integration with MoQ streaming protocol

## Features

- `opencv` - Enables OpenCV integration for working with Mat objects

## Example

```rust,no_run
use anyhow::Result;
use ffutility::{
    encoders::{FfmpegOptions, InputType}, 
    streams::{V4lH264Stream, V4lH264Config}
};

// This is just a usage example and won't actually run in doctests
fn example() -> Result<()> {
    // Configure V4L2 device for H.264 streaming
    let v4l_config = V4lH264Config {
        output_width: 1280,
        output_height: 720,
        bitrate: 500000,
        input_type: InputType::BGR24,
        v4l_fourcc: v4l::FourCC::new(b"BGR3"),
        video_dev: String::from("/dev/video0"),
    };

    // Set FFmpeg encoding options
    let mut ffmpeg_opts = FfmpegOptions::new();
    ffmpeg_opts.push((String::from("preset"), String::from("superfast")));

    // Create the stream
    let stream = V4lH264Stream::new(v4l_config, ffmpeg_opts)?;

    // Now you can use the stream to receive H.264 encoded frames
    // See the examples directory for more detailed examples
    Ok(())
}
```

*/

/// Video stream parsers, including H.264 Annex B format parsing and MoQ integration
pub mod parsers;
/// Video encoders, including H.264 encoding with various backend options
pub mod encoders;
/// Video streaming utilities, including V4L2 device capture
pub mod streams;
