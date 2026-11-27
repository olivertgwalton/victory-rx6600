//! Platform-level data formats, shared by every Xbox Series title.
//!
//! Each module is pure (bytes in, bytes out, no I/O): [`texture`] (the
//! GPU's tiled texture layouts and copies to and from linear rows),
//! [`bcpack`] (packed BC block streams), [`zlib`] (the streams DirectStorage
//! inflates) and [`xma`] (audio).

pub mod bcpack;
pub mod texture;
pub mod xma;
pub mod zlib;
