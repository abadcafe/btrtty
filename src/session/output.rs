use std::future::poll_fn;
use std::io;
use std::pin::Pin;
use std::task::Poll;

use tokio::io::{AsyncRead, AsyncReadExt, ReadBuf};

/// Turn an async byte reader into bounded, immediately coalesced output batches.
pub(super) struct OutputReader<R> {
    reader: R,
    closed: bool,
    deferred_error: Option<io::Error>,
}

impl<R> OutputReader<R>
where
    R: AsyncRead + Unpin,
{
    pub(super) fn new(reader: R) -> Self {
        Self {
            reader,
            closed: false,
            deferred_error: None,
        }
    }

    /// Wait for the first fragment, then drain only bytes that are already
    /// readable. Tail polling always completes in the current task poll; it
    /// never waits for a future fragment.
    pub(super) async fn read_batch(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "PTY output batch buffer must not be empty",
            ));
        }
        if let Some(error) = self.deferred_error.take() {
            return Err(error);
        }
        if self.closed {
            return Ok(0);
        }

        let mut count = self.reader.read(buffer).await?;
        if count == 0 {
            self.closed = true;
            return Ok(0);
        }

        while count < buffer.len() {
            match read_immediately(&mut self.reader, &mut buffer[count..]).await {
                Ok(Some(0)) => {
                    self.closed = true;
                    break;
                }
                Ok(Some(read)) => count += read,
                Ok(None) => break,
                Err(error) => {
                    self.deferred_error = Some(error);
                    break;
                }
            }
        }
        Ok(count)
    }
}

async fn read_immediately<R>(reader: &mut R, buffer: &mut [u8]) -> io::Result<Option<usize>>
where
    R: AsyncRead + Unpin,
{
    poll_fn(|context| {
        let mut read_buffer = ReadBuf::new(&mut *buffer);
        match Pin::new(&mut *reader).poll_read(context, &mut read_buffer) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(Some(read_buffer.filled().len()))),
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
            Poll::Pending => Poll::Ready(Ok(None)),
        }
    })
    .await
}
