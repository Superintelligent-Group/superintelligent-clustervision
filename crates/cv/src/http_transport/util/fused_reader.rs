use std::io::{IoSliceMut, Read, Result as IoResult};

/// Wraps another reader and provides "fused" behavior.
/// When the underlying reader reaches EOF, it is dropped
/// and the fused reader becomes an empty stub.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FusedReader<R: Read> {
    inner: Option<R>,
}

impl<R: Read> FusedReader<R> {
    pub fn new(inner: R) -> Self {
        Self { inner: Some(inner) }
    }

    #[allow(dead_code)]
    pub fn into_inner(self) -> Option<R> {
        self.inner
    }
}

impl<R: Read> Read for FusedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> IoResult<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        match &mut self.inner {
            Some(r) => {
                let l = r.read(buf)?;
                if l == 0 {
                    self.inner = None;
                }
                Ok(l)
            }
            None => Ok(0),
        }
    }

    fn read_vectored(&mut self, bufs: &mut [IoSliceMut<'_>]) -> IoResult<usize> {
        if bufs.iter().all(|buf| buf.is_empty()) {
            return Ok(0);
        }
        match &mut self.inner {
            Some(r) => {
                let l = r.read_vectored(bufs)?;
                if l == 0 {
                    self.inner = None;
                }
                Ok(l)
            }
            None => Ok(0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn empty_reads_do_not_discard_the_request_body() {
        let mut reader = FusedReader::new(std::io::Cursor::new(b"body"));
        assert_eq!(reader.read(&mut []).unwrap(), 0);
        assert_eq!(reader.read_vectored(&mut [IoSliceMut::new(&mut [])]).unwrap(), 0);
        let mut body = String::new();
        reader.read_to_string(&mut body).unwrap();
        assert_eq!(body, "body");
    }
}
