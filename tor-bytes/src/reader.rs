//! Internal: Declare the Reader type for tor-bytes

use crate::{Error, Readable, Result};
use arrayref::array_ref;

/// A type for reading messages from a slice of bytes.
///
/// Unlike io::Read, this object has a simpler error type, and is designed
/// for in-memory parsing only.
///
/// # Examples
///
/// You can use a Reader to extract information byte-by-byte:
///
/// ```
/// use tor_bytes::{Reader,Result};
/// let msg = [ 0x00, 0x01, 0x23, 0x45, 0x22, 0x00, 0x00, 0x00 ];
/// let mut r = Reader::from_slice(&msg[..]);
/// // Multi-byte values are always big-endian.
/// assert_eq!(r.take_u32()?, 0x12345);
/// assert_eq!(r.take_u8()?, 0x22);
///
/// // You can check on the length of the message...
/// assert_eq!(r.total_len(), 8);
/// assert_eq!(r.consumed(), 5);
/// assert_eq!(r.remaining(), 3);
/// // then skip over a some bytes...
/// r.advance(3)?;
/// // ... and check that the message is really exhausted.
/// r.should_be_exhausted()?;
/// # Result::Ok(())
/// ```
///
/// You can also use a Reader to extract objects that implement Readable.
/// ```
/// use tor_bytes::{Reader,Result,Readable};
/// use std::net::Ipv4Addr;
/// let msg = [ 0x00, 0x04, 0x7f, 0x00, 0x00, 0x01];
/// let mut r = Reader::from_slice(&msg[..]);
///
/// let tp: u16 = r.extract()?;
/// let ip: Ipv4Addr = r.extract()?;
/// assert_eq!(tp, 4);
/// assert_eq!(ip, Ipv4Addr::LOCALHOST);
/// # Result::Ok(())
/// ```
pub struct Reader<'a> {
    /// The underlying slice that we're reading from
    b: &'a [u8],
    /// The next position in the slice that we intend to read from.
    off: usize,
}

impl<'a> Reader<'a> {
    /// Construct a new Reader from a slice of bytes.
    pub fn from_slice(slice: &'a [u8]) -> Self {
        Reader { b: slice, off: 0 }
    }
    /// Construct a new Reader from a 'Bytes' object.
    pub fn from_bytes(b: &'a bytes::Bytes) -> Self {
        Self::from_slice(b.as_ref())
    }
    /// Return the total length of the slice in this reader, including
    /// consumed bytes and remaining bytes.
    pub fn total_len(&self) -> usize {
        self.b.len()
    }
    /// Return the total number of bytes in this reader that have not
    /// yet been read.
    pub fn remaining(&self) -> usize {
        self.b.len() - self.off
    }
    /// Consume this reader, and return a slice containing the remaining
    /// bytes from its slice that it did not consume.
    pub fn into_rest(self) -> &'a [u8] {
        &self.b[self.off..]
    }
    /// Return the total number of bytes in this reader that have
    /// already been read.
    pub fn consumed(&self) -> usize {
        self.off
    }
    /// Skip `n` bytes from the reader.
    ///
    /// Returns Ok on success.  Returns Err(Errror::Truncated) if there were
    /// not enough bytes to skip.
    pub fn advance(&mut self, n: usize) -> Result<()> {
        if n > self.remaining() {
            return Err(Error::Truncated);
        }
        self.off += n;
        Ok(())
    }
    /// Check whether this reader is exhausted (out of bytes).
    ///
    /// Return Ok if it is, and Err(Error::ExtraneousBytes)
    /// if there were extra bytes.
    pub fn should_be_exhausted(&self) -> Result<()> {
        if self.remaining() != 0 {
            return Err(Error::ExtraneousBytes);
        }
        Ok(())
    }
    /// Truncate this reader, so that no more than `n` bytes remain.
    ///
    /// Fewer than `n` bytes may remain if there were not enough bytes
    /// to begin with.
    pub fn truncate(&mut self, n: usize) {
        self.b = &self.b[..self.off + n];
    }
    /// Try to return a slice of `n` bytes from this reader without
    /// consuming them.
    ///
    /// On success, returns Ok(slice).  If there are fewer than n
    /// bytes, returns Err(Error::Truncated).
    pub fn peek(&self, n: usize) -> Result<&'a [u8]> {
        if self.remaining() < n {
            return Err(Error::Truncated);
        }

        Ok(&self.b[self.off..(n + self.off)])
    }
    /// Try to consume and return a slice of `n` bytes from this reader.
    ///
    /// On success, returns Ok(Slice).  If there are fewer than n
    /// bytes, returns Err(Error::Truncated).
    ///
    /// # Example
    /// ```
    /// use tor_bytes::{Reader,Result};
    /// let m = b"Hello World";
    /// let mut r = Reader::from_slice(m);
    /// assert_eq!(r.take(5)?, b"Hello");
    /// assert_eq!(r.take_u8()?, 0x20);
    /// assert_eq!(r.take(5)?, b"World");
    /// r.should_be_exhausted()?;
    /// # Result::Ok(())
    /// ```
    pub fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let b = self.peek(n)?;
        self.advance(n)?;
        Ok(b)
    }
    /// Try to consume and return a u8 from this reader.
    pub fn take_u8(&mut self) -> Result<u8> {
        let b = self.take(1)?;
        Ok(b[0])
    }
    /// Try to consume and return a big-endian u16 from this reader.
    pub fn take_u16(&mut self) -> Result<u16> {
        let b = self.take(2)?;
        let r = u16::from_be_bytes(*array_ref![b, 0, 2]);
        Ok(r)
    }
    /// Try to consume and return a big-endian u32 from this reader.
    pub fn take_u32(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        let r = u32::from_be_bytes(*array_ref![b, 0, 4]);
        Ok(r)
    }
    /// Try to consume and return a big-endian u64 from this reader.
    pub fn take_u64(&mut self) -> Result<u64> {
        let b = self.take(8)?;
        let r = u64::from_be_bytes(*array_ref![b, 0, 8]);
        Ok(r)
    }
    /// Try to consume and return a big-endian u128 from this reader.
    pub fn take_u128(&mut self) -> Result<u128> {
        let b = self.take(16)?;
        let r = u128::from_be_bytes(*array_ref![b, 0, 16]);
        Ok(r)
    }
    /// Try to consume and return bytes from this buffer until we
    /// encounter a terminating byte equal to `term`.
    ///
    /// On success, returns Ok(Slice), where the slice does not
    /// include the terminating byte.  Returns Err(Error::Truncated)
    /// if we do not find the terminating bytes.
    ///
    /// Advances the reader to the point immediately after the terminating
    /// byte.
    ///
    /// # Example
    /// ```
    /// use tor_bytes::{Reader,Result};
    /// let m = b"Hello\0wrld";
    /// let mut r = Reader::from_slice(m);
    /// assert_eq!(r.take_until(0)?, b"Hello");
    /// assert_eq!(r.into_rest(), b"wrld");
    /// # Result::Ok(())
    /// ```
    pub fn take_until(&mut self, term: u8) -> Result<&'a [u8]> {
        let pos = self.b[self.off..]
            .iter()
            .position(|b| *b == term)
            .ok_or(Error::Truncated)?;
        let result = self.take(pos)?;
        self.advance(1)?;
        Ok(result)
    }
    /// Try to decode and remove a Readable from this reader, using its
    /// take_from() method.
    ///
    /// On failure, consumes nothing.
    pub fn extract<E: Readable>(&mut self) -> Result<E> {
        let off_orig = self.off;
        let result = E::take_from(self);
        if result.is_err() {
            // We encountered an error; we should rewind.
            self.off = off_orig;
        }
        result
    }

    /// Try to decode and remove `n` Readables from this reader, using the
    /// Readable's take_from() method.
    ///
    /// On failure, consumes nothing.
    pub fn extract_n<E: Readable>(&mut self, n: usize) -> Result<Vec<E>> {
        let mut result = Vec::new();
        let off_orig = self.off;
        for _ in 0..n {
            match E::take_from(self) {
                Ok(item) => result.push(item),
                Err(e) => {
                    // Encountered an error; we should rewind.
                    self.off = off_orig;
                    return Err(e);
                }
            }
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bytecursor_read_ok() {
        let bytes = b"On a mountain halfway between Reno and Rome";
        let mut bc = Reader::from_slice(&bytes[..]);

        assert_eq!(bc.consumed(), 0);
        assert_eq!(bc.remaining(), 43);
        assert_eq!(bc.total_len(), 43);

        assert_eq!(bc.take(3).unwrap(), &b"On "[..]);
        assert_eq!(bc.consumed(), 3);

        assert_eq!(bc.take_u16().unwrap(), 0x6120);
        assert_eq!(bc.take_u8().unwrap(), 0x6d);
        assert_eq!(bc.take_u64().unwrap(), 0x6f756e7461696e20);
        assert_eq!(bc.take_u32().unwrap(), 0x68616c66);
        assert_eq!(bc.consumed(), 18);
        assert_eq!(bc.remaining(), 25);
        assert_eq!(bc.total_len(), 43);

        assert_eq!(bc.peek(7).unwrap(), &b"way bet"[..]);
        assert_eq!(bc.consumed(), 18); // no change
        assert_eq!(bc.remaining(), 25); // no change
        assert_eq!(bc.total_len(), 43); // no change

        assert_eq!(bc.peek(7).unwrap(), &b"way bet"[..]);
        assert_eq!(bc.consumed(), 18); // no change this time either.

        bc.advance(12).unwrap();
        assert_eq!(bc.consumed(), 30);
        assert_eq!(bc.remaining(), 13);

        let rem = bc.into_rest();
        assert_eq!(rem, &b"Reno and Rome"[..]);

        // now let's try consuming right up to the end.
        let mut bc = Reader::from_slice(&bytes[..]);
        bc.advance(22).unwrap();
        assert_eq!(bc.remaining(), 21);
        let rem = bc.take(21).unwrap();
        assert_eq!(rem, &b"between Reno and Rome"[..]);
        assert_eq!(bc.consumed(), 43);
        assert_eq!(bc.remaining(), 0);

        // We can still take a zero-length slice.
        assert_eq!(bc.take(0).unwrap(), &b""[..]);
    }

    #[test]
    fn read_u128() {
        let bytes = bytes::Bytes::from(&b"irreproducibility?"[..]); // 18 bytes
        let mut r = Reader::from_bytes(&bytes);

        assert_eq!(r.take_u8().unwrap(), b'i');
        assert_eq!(r.take_u128().unwrap(), 0x72726570726f6475636962696c697479);
        assert_eq!(r.remaining(), 1);
    }

    #[test]
    fn bytecursor_read_missing() {
        let bytes = b"1234567";
        let mut bc = Reader::from_slice(&bytes[..]);

        assert_eq!(bc.consumed(), 0);
        assert_eq!(bc.remaining(), 7);
        assert_eq!(bc.total_len(), 7);

        assert_eq!(bc.take_u64(), Err(Error::Truncated));
        assert_eq!(bc.take(8), Err(Error::Truncated));
        assert_eq!(bc.peek(8), Err(Error::Truncated));

        assert_eq!(bc.consumed(), 0);
        assert_eq!(bc.remaining(), 7);
        assert_eq!(bc.total_len(), 7);

        assert_eq!(bc.take_u32().unwrap(), 0x31323334); // get 4 bytes. 3 left.
        assert_eq!(bc.take_u32(), Err(Error::Truncated));

        assert_eq!(bc.consumed(), 4);
        assert_eq!(bc.remaining(), 3);
        assert_eq!(bc.total_len(), 7);

        assert_eq!(bc.take_u16().unwrap(), 0x3536); // get 2 bytes. 1 left.
        assert_eq!(bc.take_u16(), Err(Error::Truncated));

        assert_eq!(bc.consumed(), 6);
        assert_eq!(bc.remaining(), 1);
        assert_eq!(bc.total_len(), 7);

        assert_eq!(bc.take_u8().unwrap(), 0x37); // get 1 byte. 0 left.
        assert_eq!(bc.take_u8(), Err(Error::Truncated));

        assert_eq!(bc.consumed(), 7);
        assert_eq!(bc.remaining(), 0);
        assert_eq!(bc.total_len(), 7);
    }

    #[test]
    fn advance_too_far() {
        let bytes = b"12345";
        let mut r = Reader::from_slice(&bytes[..]);
        assert_eq!(r.remaining(), 5);
        assert_eq!(r.advance(6), Err(Error::Truncated));
        assert_eq!(r.remaining(), 5);
        assert_eq!(r.advance(5), Ok(()));
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn truncate() {
        let bytes = b"Hello universe!!!1!";
        let mut r = Reader::from_slice(&bytes[..]);

        assert_eq!(r.take(5).unwrap(), &b"Hello"[..]);
        assert_eq!(r.remaining(), 14);
        assert_eq!(r.consumed(), 5);
        r.truncate(9);
        assert_eq!(r.remaining(), 9);
        assert_eq!(r.consumed(), 5);
        assert_eq!(r.take_u8().unwrap(), 0x20);
        assert_eq!(r.into_rest(), &b"universe"[..]);
    }

    #[test]
    fn exhaust() {
        let r = Reader::from_slice(&b""[..]);
        assert_eq!(r.should_be_exhausted(), Ok(()));

        let mut r = Reader::from_slice(&b"outis"[..]);
        assert_eq!(r.should_be_exhausted(), Err(Error::ExtraneousBytes));
        r.take(4).unwrap();
        assert_eq!(r.should_be_exhausted(), Err(Error::ExtraneousBytes));
        r.take(1).unwrap();
        assert_eq!(r.should_be_exhausted(), Ok(()));
    }

    #[test]
    fn take_until() {
        let mut r = Reader::from_slice(&b"si vales valeo"[..]);
        assert_eq!(r.take_until(b' ').unwrap(), &b"si"[..]);
        assert_eq!(r.take_until(b' ').unwrap(), &b"vales"[..]);
        assert_eq!(r.take_until(b' '), Err(Error::Truncated));
    }

    #[test]
    fn extract() {
        // For example purposes, declare a length-then-bytes string type.
        #[derive(Debug)]
        struct LenEnc(Vec<u8>);
        impl Readable for LenEnc {
            fn take_from(b: &mut Reader<'_>) -> Result<Self> {
                let length = b.take_u8()?;
                let content = b.take(length as usize)?.into();
                Ok(LenEnc(content))
            }
        }

        let bytes = b"\x04this\x02is\x09sometimes\x01a\x06string!";
        let mut r = Reader::from_slice(&bytes[..]);

        let le: LenEnc = r.extract().unwrap();
        assert_eq!(&le.0[..], &b"this"[..]);

        let les: Vec<LenEnc> = r.extract_n(4).unwrap();
        assert_eq!(&les[3].0[..], &b"string"[..]);

        assert_eq!(r.remaining(), 1);

        // Make sure that we don't advance on a failing extract().
        let le: Result<LenEnc> = r.extract();
        assert_eq!(le.unwrap_err(), Error::Truncated);
        assert_eq!(r.remaining(), 1);

        // Make sure that we don't advance on a failing extract_n()
        let mut r = Reader::from_slice(&bytes[..]);
        assert_eq!(r.remaining(), 28);
        let les: Result<Vec<LenEnc>> = r.extract_n(10);
        assert_eq!(les.unwrap_err(), Error::Truncated);
        assert_eq!(r.remaining(), 28);
    }
}
