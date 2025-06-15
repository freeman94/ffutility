/*! 
Video stream parsing modules for various formats.

This module provides parsers for video streams, currently focusing on H.264
with support for Annex B format and integration with MoQ streaming protocol.
*/

mod h264;
pub use h264::*;
