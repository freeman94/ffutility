/*! 
Video streaming modules for various devices and formats.

This module provides utilities for capturing video streams from different sources,
currently focusing on V4L2 devices with H.264 encoding.
*/

mod v4l_h264;
pub use v4l_h264::*;
