//! zlib (RFC 1950) streams, as DirectStorage inflates them for a request:
//! one whole stream into a buffer of the request's size.

use std::fmt;

/// Why a stream did not inflate: the inflater's status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Error(pub &'static str);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "zlib: {}", self.0)
    }
}

impl std::error::Error for Error {}

/// An inflater, kept between streams (its window and tables are reused).
pub struct Inflater(zlib_rs::Inflate);

impl Default for Inflater {
    fn default() -> Inflater {
        Inflater::new()
    }
}

impl Inflater {
    pub fn new() -> Inflater {
        Inflater(zlib_rs::Inflate::new(true, 15))
    }

    /// Inflates one stream into `out`, returning the bytes written. A stream
    /// that ends short of `out` passes (nothing says it must fill it); one
    /// that needs more room, is cut short, or is damaged fails.
    pub fn inflate(&mut self, source: &[u8], out: &mut [u8]) -> Result<usize, Error> {
        let state = &mut self.0;
        state.reset(true);
        // `decompress` cannot tell a valid empty stream from a full buffer
        // when there is no room at all, so an empty `out` gets one scratch
        // byte and the count is checked afterwards.
        let out_len = out.len();
        let mut scratch = [0u8; 1];
        let dest = if out_len == 0 { &mut scratch[..] } else { out };
        let (mut in_pos, mut out_pos) = (0usize, 0usize);
        let status = loop {
            match state.decompress(&source[in_pos..], &mut dest[out_pos..], zlib_rs::InflateFlush::NoFlush) {
                Ok(zlib_rs::Status::StreamEnd) => break Ok(()),
                Ok(zlib_rs::Status::Ok) => {
                    let next_in = state.total_in() as usize;
                    let next_out = state.total_out() as usize;
                    if next_in == in_pos && next_out == out_pos {
                        break Err(Error("no progress (the stream needs more room or input)"));
                    }
                    in_pos = next_in;
                    out_pos = next_out;
                }
                Ok(_) => break Err(Error("the stream stopped short")),
                Err(_) => break Err(Error("damaged stream")),
            }
        };
        let written = state.total_out() as usize;
        match status {
            Ok(()) if written <= out_len => Ok(written),
            Ok(()) => Err(Error("an empty buffer for a stream with data")),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stored (uncompressed) zlib stream of `data`.
    fn stored(data: &[u8]) -> Vec<u8> {
        let mut s = vec![0x78, 0x01, 1];
        s.extend((data.len() as u16).to_le_bytes());
        s.extend((!(data.len() as u16)).to_le_bytes());
        s.extend(data);
        let (mut a, mut b) = (1u32, 0u32);
        for &x in data {
            a = (a + u32::from(x)) % 65521;
            b = (b + a) % 65521;
        }
        s.extend(((b << 16) | a).to_be_bytes());
        s
    }

    #[test]
    fn whole_short_and_failing_streams() {
        let mut z = Inflater::new();
        let s = stored(b"hello");
        let mut out = [0u8; 8];
        assert_eq!(z.inflate(&s, &mut out), Ok(5));
        assert_eq!(&out[..5], b"hello");
        let mut small = [0u8; 3];
        assert!(z.inflate(&s, &mut small).is_err());
        assert!(z.inflate(&s[..6], &mut out).is_err());
        assert_eq!(z.inflate(&stored(b""), &mut []), Ok(0));
        assert!(z.inflate(&s, &mut []).is_err());
        let mut bad = s.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert!(z.inflate(&bad, &mut out).is_err());
        // The state is reused: a good stream after a bad one.
        assert_eq!(z.inflate(&s, &mut out), Ok(5));
    }
}
