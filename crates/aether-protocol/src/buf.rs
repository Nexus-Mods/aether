//! Where an encoder writes: [`Buf`], implemented for `Vec<u8>` (grows) and
//! for [`SliceBuf`] (a fixed slice, such as a ring slot's payload, that
//! records an overflow instead of growing).

/// A sink for encoded bytes. Encoders only append.
pub trait Buf {
    /// Appends `bytes`.
    fn put(&mut self, bytes: &[u8]);
}

impl Buf for Vec<u8> {
    fn put(&mut self, bytes: &[u8]) {
        self.extend_from_slice(bytes);
    }
}

impl<B: Buf + ?Sized> Buf for &mut B {
    fn put(&mut self, bytes: &[u8]) {
        (**self).put(bytes);
    }
}

/// The encoded bytes did not fit a [`SliceBuf`]: `needed` is the length the
/// whole message wanted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Overflow {
    pub needed: usize,
}

impl core::fmt::Display for Overflow {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "encoded message needs {} bytes", self.needed)
    }
}

impl std::error::Error for Overflow {}

/// A fixed-capacity writer over `&mut [u8]`. A `put` that does not fit writes
/// nothing and marks the buffer overflowed; later puts only count, so
/// [`SliceBuf::finish`] can say how long the message would have been.
#[derive(Debug)]
pub struct SliceBuf<'a> {
    buf: &'a mut [u8],
    len: usize,
    overflowed: bool,
}

impl<'a> SliceBuf<'a> {
    pub fn new(buf: &'a mut [u8]) -> Self {
        SliceBuf {
            buf,
            len: 0,
            overflowed: false,
        }
    }

    /// Bytes put so far, counting those that did not fit.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn overflowed(&self) -> bool {
        self.overflowed
    }

    /// The length written, or [`Overflow`] if anything did not fit.
    pub fn finish(self) -> Result<usize, Overflow> {
        if self.overflowed {
            Err(Overflow { needed: self.len })
        } else {
            Ok(self.len)
        }
    }
}

impl Buf for SliceBuf<'_> {
    fn put(&mut self, bytes: &[u8]) {
        let end = self.len.saturating_add(bytes.len());
        if !self.overflowed && end <= self.buf.len() {
            self.buf[self.len..end].copy_from_slice(bytes);
        } else {
            self.overflowed = true;
        }
        self.len = end;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slice_buf_writes_until_full_then_reports_the_length_needed() {
        let mut mem = [0u8; 4];
        let mut b = SliceBuf::new(&mut mem);
        b.put(&[1, 2]);
        b.put(&[3, 4]);
        assert_eq!(b.finish(), Ok(4));
        assert_eq!(mem, [1, 2, 3, 4]);

        let mut mem = [0u8; 3];
        let mut b = SliceBuf::new(&mut mem);
        b.put(&[1, 2]);
        b.put(&[3, 4]);
        b.put(&[5]);
        assert!(b.overflowed());
        assert_eq!(b.finish(), Err(Overflow { needed: 5 }));
        assert_eq!(mem, [1, 2, 0], "a put that does not fit writes nothing");
    }
}
