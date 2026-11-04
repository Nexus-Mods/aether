//! The byte cursor the codecs decode with, and the error every decoder
//! returns.

use aether_registry::utf16_len;

/// A message, opcode or status that does not decode: truncated, trailing
/// bytes where the message has none, bad UTF-8, a value out of range, or an
/// unknown number. Opaque; `Display` says which.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct DecodeError {
    what: &'static str,
}

impl DecodeError {
    pub(crate) const fn new(what: &'static str) -> Self {
        DecodeError { what }
    }
}

impl core::fmt::Debug for DecodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "DecodeError({})", self.what)
    }
}

impl core::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "malformed message: {}", self.what)
    }
}

impl std::error::Error for DecodeError {}

pub(crate) const SHORT: DecodeError = DecodeError::new("truncated");

/// Bounds-checked cursor over a payload.
pub(crate) struct Rd<'a>(pub(crate) &'a [u8]);

impl<'a> Rd<'a> {
    pub(crate) fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        if self.0.len() < n {
            return Err(SHORT);
        }
        let (h, t) = self.0.split_at(n);
        self.0 = t;
        Ok(h)
    }
    pub(crate) fn array<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        let mut a = [0u8; N];
        a.copy_from_slice(self.take(N)?);
        Ok(a)
    }
    pub(crate) fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take(1)?[0])
    }
    pub(crate) fn u32(&mut self) -> Result<u32, DecodeError> {
        Ok(u32::from_le_bytes(self.array()?))
    }
    pub(crate) fn u64(&mut self) -> Result<u64, DecodeError> {
        Ok(u64::from_le_bytes(self.array()?))
    }
    /// A `len:u32` prefix and that many bytes, refused above `max`.
    pub(crate) fn bytes(&mut self, max: usize) -> Result<&'a [u8], DecodeError> {
        let n = self.u32()? as usize;
        if n > max {
            return Err(DecodeError::new("length over limit"));
        }
        self.take(n)
    }
    pub(crate) fn utf8(b: &'a [u8]) -> Result<&'a str, DecodeError> {
        core::str::from_utf8(b).map_err(|_| DecodeError::new("bad utf-8"))
    }
    /// Everything that is left.
    pub(crate) fn rest(&mut self) -> &'a [u8] {
        let rest = self.0;
        self.0 = &[];
        rest
    }
    /// A string whose length in UTF-16 units is at most `max_units`.
    pub(crate) fn check_utf16(s: &str, max_units: usize) -> Result<(), DecodeError> {
        if utf16_len(s) <= max_units {
            Ok(())
        } else {
            Err(DecodeError::new("name over limit"))
        }
    }
    pub(crate) fn done(&self) -> Result<(), DecodeError> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(DecodeError::new("trailing bytes"))
        }
    }
}
