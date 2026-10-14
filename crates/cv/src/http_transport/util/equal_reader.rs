use std::io::Result as IoResult;
use std::io::{Error, ErrorKind, Read};
use std::sync::mpsc::channel;
use std::sync::mpsc::{Receiver, Sender};

/// A `Reader` that reads exactly the number of bytes from a sub-reader.
///
/// If the limit is reached, it returns EOF. If the limit is not reached
/// when the destructor is called, the remaining bytes will be read and
/// thrown away.
pub struct EqualReader<R>
where
    R: Read,
{
    reader: R,
    size: usize,
    last_read_signal: Sender<IoResult<()>>,
}

impl<R> EqualReader<R>
where
    R: Read,
{
    pub fn new(reader: R, size: usize) -> (EqualReader<R>, Receiver<IoResult<()>>) {
        let (tx, rx) = channel();

        let r = EqualReader {
            reader,
            size,
            last_read_signal: tx,
        };

        (r, rx)
    }
}

impl<R> Read for EqualReader<R>
where
    R: Read,
{
    fn read(&mut self, buf: &mut [u8]) -> IoResult<usize> {
        if self.size == 0 || buf.is_empty() {
            return Ok(0);
        }

        let buf = if buf.len() < self.size {
            buf
        } else {
            &mut buf[..self.size]
        };

        match self.reader.read(buf) {
            Ok(0) => Err(Error::new(ErrorKind::UnexpectedEof, "body ended before Content-Length")),
            Ok(len) => {
                self.size -= len;
                Ok(len)
            }
            err @ Err(_) => err,
        }
    }
}

impl<R> Drop for EqualReader<R>
where
    R: Read,
{
    fn drop(&mut self) {
        let mut remaining_to_read = self.size;

        let mut buf = [0u8; 8192];
        while remaining_to_read > 0 {
            let capacity = remaining_to_read.min(buf.len());
            match self.reader.read(&mut buf[..capacity]) {
                Err(e) => {
                    self.last_read_signal.send(Err(e)).ok();
                    break;
                }
                Ok(0) => {
                    self.last_read_signal
                        .send(Err(Error::new(
                            ErrorKind::UnexpectedEof,
                            "body ended before Content-Length",
                        )))
                        .ok();
                    break;
                }
                Ok(other) => {
                    remaining_to_read -= other;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::EqualReader;
    use std::io::Read;

    #[test]
    fn truncated_body_fails_and_discarding_uses_bounded_scratch() {
        let (mut reader, _) = EqualReader::new(std::io::Cursor::new(b"{}"), 1025);
        let error = reader.read_to_string(&mut String::new()).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
        struct Inspect(std::sync::Arc<std::sync::atomic::AtomicUsize>);
        impl Read for Inspect {
            fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
                self.0.store(bytes.len(), std::sync::atomic::Ordering::Relaxed);
                Ok(0)
            }
        }
        let offered = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (reader, signal) = EqualReader::new(Inspect(offered.clone()), 1024 * 1024);
        drop(reader);
        assert!(offered.load(std::sync::atomic::Ordering::Relaxed) <= 8192);
        assert_eq!(
            signal.recv().unwrap().unwrap_err().kind(),
            std::io::ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn test_limit() {
        use std::io::Cursor;

        let mut org_reader = Cursor::new("hello world".to_string().into_bytes());

        {
            let (mut equal_reader, _) = EqualReader::new(org_reader.by_ref(), 5);

            let mut string = String::new();
            equal_reader.read_to_string(&mut string).unwrap();
            assert_eq!(string, "hello");
        }

        let mut string = String::new();
        org_reader.read_to_string(&mut string).unwrap();
        assert_eq!(string, " world");
    }

    #[test]
    fn test_not_enough() {
        use std::io::Cursor;

        let mut org_reader = Cursor::new("hello world".to_string().into_bytes());

        {
            let (mut equal_reader, _) = EqualReader::new(org_reader.by_ref(), 5);

            let mut vec = [0];
            equal_reader.read_exact(&mut vec).unwrap();
            assert_eq!(vec[0], b'h');
        }

        let mut string = String::new();
        org_reader.read_to_string(&mut string).unwrap();
        assert_eq!(string, " world");
    }
}
