/*! 
Video encoding modules for various formats.

This module provides encoders for different video formats, currently focusing on H.264
encoding with support for multiple hardware-accelerated backends.
*/

mod h264;
pub use h264::*;
