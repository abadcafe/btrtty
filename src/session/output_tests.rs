use std::collections::VecDeque;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, ReadBuf};

use super::output::OutputReader;

#[tokio::test]
async fn test_read_batch_waits_for_first_fragment_and_drains_ready_tail() {
    let reader = ScriptedReader::new([
        ReadStep::PendingOnce,
        ReadStep::Data(b"ab".to_vec()),
        ReadStep::Data(b"cd".to_vec()),
        ReadStep::Data(b"ef".to_vec()),
        ReadStep::Pending,
    ]);
    let mut output = OutputReader::new(reader);
    let mut buffer = [0_u8; 8];

    let count = output.read_batch(&mut buffer).await.unwrap();

    assert_eq!(count, 6);
    assert_eq!(&buffer[..count], b"abcdef");
}

#[tokio::test]
async fn test_read_batch_returns_without_waiting_for_unready_tail() {
    let reader = ScriptedReader::new([ReadStep::Data(b"ready".to_vec()), ReadStep::Pending]);
    let mut output = OutputReader::new(reader);
    let mut buffer = [0_u8; 16];

    let count = tokio::time::timeout(Duration::from_millis(20), output.read_batch(&mut buffer))
        .await
        .expect("ready-tail drain waited for future data")
        .unwrap();

    assert_eq!(count, 5);
    assert_eq!(&buffer[..count], b"ready");
}

#[tokio::test]
async fn test_read_batch_stops_at_capacity_without_consuming_the_next_fragment() {
    let reader = ScriptedReader::new([
        ReadStep::Data(b"ab".to_vec()),
        ReadStep::Data(b"cd".to_vec()),
        ReadStep::Data(b"ef".to_vec()),
        ReadStep::Pending,
    ]);
    let mut output = OutputReader::new(reader);
    let mut first = [0_u8; 4];
    let mut second = [0_u8; 4];

    let first_count = output.read_batch(&mut first).await.unwrap();
    let second_count = output.read_batch(&mut second).await.unwrap();

    assert_eq!(first_count, 4);
    assert_eq!(&first, b"abcd");
    assert_eq!(second_count, 2);
    assert_eq!(&second[..second_count], b"ef");
}

#[tokio::test]
async fn test_read_batch_defers_tail_error_until_drained_bytes_are_returned() {
    let reader = ScriptedReader::new([ReadStep::Data(b"final".to_vec()), ReadStep::Error]);
    let mut output = OutputReader::new(reader);
    let mut buffer = [0_u8; 16];

    let count = output.read_batch(&mut buffer).await.unwrap();
    let error = output.read_batch(&mut buffer).await.unwrap_err();

    assert_eq!(count, 5);
    assert_eq!(&buffer[..count], b"final");
    assert_eq!(error.kind(), io::ErrorKind::Other);
}

#[tokio::test]
async fn test_read_batch_remembers_eof_after_drained_bytes() {
    let reader = ScriptedReader::new([ReadStep::Data(b"final".to_vec()), ReadStep::Eof]);
    let mut output = OutputReader::new(reader);
    let mut buffer = [0_u8; 16];

    let count = output.read_batch(&mut buffer).await.unwrap();
    let eof = output.read_batch(&mut buffer).await.unwrap();

    assert_eq!(count, 5);
    assert_eq!(&buffer[..count], b"final");
    assert_eq!(eof, 0);
}

#[tokio::test]
async fn test_read_batch_rejects_an_empty_destination() {
    let reader = ScriptedReader::new([ReadStep::Data(b"x".to_vec())]);
    let mut output = OutputReader::new(reader);

    let error = output.read_batch(&mut []).await.unwrap_err();

    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
}

enum ReadStep {
    Data(Vec<u8>),
    PendingOnce,
    Pending,
    Eof,
    Error,
}

struct ScriptedReader {
    steps: VecDeque<ReadStep>,
}

impl ScriptedReader {
    fn new(steps: impl IntoIterator<Item = ReadStep>) -> Self {
        Self {
            steps: steps.into_iter().collect(),
        }
    }
}

impl AsyncRead for ScriptedReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let step = self.steps.pop_front().unwrap_or(ReadStep::Eof);
        match step {
            ReadStep::Data(mut data) => {
                let count = buffer.remaining().min(data.len());
                buffer.put_slice(&data[..count]);
                if count < data.len() {
                    data.drain(..count);
                    self.steps.push_front(ReadStep::Data(data));
                }
                Poll::Ready(Ok(()))
            }
            ReadStep::PendingOnce => {
                context.waker().wake_by_ref();
                Poll::Pending
            }
            ReadStep::Pending => {
                self.steps.push_front(ReadStep::Pending);
                Poll::Pending
            }
            ReadStep::Eof => Poll::Ready(Ok(())),
            ReadStep::Error => Poll::Ready(Err(io::Error::other("scripted read failure"))),
        }
    }
}
