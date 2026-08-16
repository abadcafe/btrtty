use std::io;
use std::time::Duration;

use bytes::Bytes;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::time::Instant;

const COALESCE_DELAY: Duration = Duration::from_millis(1);

/// Convert arbitrarily short PTY reads into bounded RFCOMM-sized chunks.
///
/// The first byte starts an absolute deadline. Readiness before that deadline
/// fills the same chunk, while a quiet or continuously trickling source cannot
/// delay delivery beyond [`COALESCE_DELAY`].
pub(super) struct OutputReader<R> {
    reader: R,
    buffer: Vec<u8>,
    eof: bool,
}

impl<R: AsyncRead + Unpin> OutputReader<R> {
    pub(super) fn new(reader: R, chunk_size: usize) -> io::Result<Self> {
        if chunk_size == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "PTY output chunk size must be non-zero",
            ));
        }
        Ok(Self {
            reader,
            buffer: vec![0; chunk_size],
            eof: false,
        })
    }

    pub(super) async fn read_chunk(&mut self) -> io::Result<Option<Bytes>> {
        if self.eof {
            return Ok(None);
        }

        let mut filled = self.reader.read(&mut self.buffer).await?;
        if filled == 0 {
            self.eof = true;
            return Ok(None);
        }

        let deadline = Instant::now() + COALESCE_DELAY;
        while filled < self.buffer.len() {
            match tokio::time::timeout_at(deadline, self.reader.read(&mut self.buffer[filled..]))
                .await
            {
                Ok(Ok(0)) => {
                    self.eof = true;
                    break;
                }
                Ok(Ok(count)) => filled += count,
                Ok(Err(error)) => return Err(error),
                Err(_) => break,
            }
        }

        Ok(Some(Bytes::copy_from_slice(&self.buffer[..filled])))
    }
}
