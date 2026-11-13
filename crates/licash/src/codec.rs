//! Framing for the explicit byte codecs of values that cross between
//! parties: big-endian integers, u32 lengths and counts checked against the
//! bytes that remain before anything is allocated, and no trailing bytes.

use crate::Error;

pub(crate) fn put_u32(bytes: &mut Vec<u8>, n: usize) -> Result<(), Error> {
    let n = u32::try_from(n).map_err(|_| Error::Protocol("length overflow".into()))?;
    bytes.extend(n.to_be_bytes());
    Ok(())
}

pub(crate) fn put_blob(bytes: &mut Vec<u8>, blob: &[u8]) -> Result<(), Error> {
    put_u32(bytes, blob.len())?;
    bytes.extend(blob);
    Ok(())
}

pub(crate) struct Reader<'a> {
    bytes: &'a [u8],
    what: &'static str,
}

impl<'a> Reader<'a> {
    /// Read `bytes`, an encoded `what` (named in errors), after its `tag`.
    pub(crate) fn new(bytes: &'a [u8], what: &'static str, tag: &[u8]) -> Result<Self, Error> {
        let mut reader = Self { bytes, what };
        if reader.take(tag.len())? != tag {
            return Err(reader.bad("tag"));
        }
        Ok(reader)
    }

    pub(crate) fn bad(&self, reason: &str) -> Error {
        Error::Protocol(format!("{} {reason}", self.what))
    }

    pub(crate) fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        if n > self.bytes.len() {
            return Err(self.bad("truncated"));
        }
        let (head, tail) = self.bytes.split_at(n);
        self.bytes = tail;
        Ok(head)
    }

    pub(crate) fn array<const N: usize>(&mut self) -> Result<[u8; N], Error> {
        Ok(self.take(N)?.try_into().expect("took N bytes"))
    }

    pub(crate) fn byte(&mut self) -> Result<u8, Error> {
        Ok(self.array::<1>()?[0])
    }

    pub(crate) fn u32(&mut self) -> Result<u32, Error> {
        Ok(u32::from_be_bytes(self.array()?))
    }

    pub(crate) fn u64(&mut self) -> Result<u64, Error> {
        Ok(u64::from_be_bytes(self.array()?))
    }

    /// A count of items of `width` bytes each, at most what remains.
    pub(crate) fn count(&mut self, width: usize) -> Result<usize, Error> {
        let n = self.u32()? as usize;
        if n > self.bytes.len() / width.max(1) {
            return Err(self.bad("count exceeds its bytes"));
        }
        Ok(n)
    }

    /// A u32-length-prefixed byte string.
    pub(crate) fn blob(&mut self) -> Result<&'a [u8], Error> {
        let n = self.u32()? as usize;
        self.take(n)
    }

    pub(crate) fn finish(self) -> Result<(), Error> {
        if !self.bytes.is_empty() {
            return Err(self.bad("trailing bytes"));
        }
        Ok(())
    }
}
