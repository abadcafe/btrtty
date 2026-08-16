use std::collections::VecDeque;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWriteExt, ReadBuf};

use super::output::OutputReader;

struct FragmentedReader {
    fragments: VecDeque<Vec<u8>>,
    offset: usize,
}

impl FragmentedReader {
    fn new(fragments: &[&[u8]]) -> Self {
        Self {
            fragments: fragments.iter().map(|fragment| fragment.to_vec()).collect(),
            offset: 0,
        }
    }
}

impl AsyncRead for FragmentedReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        while let Some(fragment) = self.fragments.front() {
            if self.offset == fragment.len() {
                self.fragments.pop_front();
                self.offset = 0;
                continue;
            }

            let count = buffer.remaining().min(fragment.len() - self.offset);
            buffer.put_slice(&fragment[self.offset..self.offset + count]);
            self.offset += count;
            return Poll::Ready(Ok(()));
        }
        Poll::Ready(Ok(()))
    }
}

#[tokio::test]
async fn test_output_reader_coalesces_ready_fragments_to_mtu() {
    let reader = FragmentedReader::new(&[b"a", b"bc", b"defgh"]);
    let mut output = OutputReader::new(reader, 4).unwrap();

    assert_eq!(&output.read_chunk().await.unwrap().unwrap()[..], b"abcd");
    assert_eq!(&output.read_chunk().await.unwrap().unwrap()[..], b"efgh");
    assert!(output.read_chunk().await.unwrap().is_none());
}

#[tokio::test]
async fn test_output_reader_flushes_partial_chunk_at_eof() {
    let reader = FragmentedReader::new(&[b"a", b"bc"]);
    let mut output = OutputReader::new(reader, 8).unwrap();

    assert_eq!(&output.read_chunk().await.unwrap().unwrap()[..], b"abc");
    assert!(output.read_chunk().await.unwrap().is_none());
}

#[tokio::test]
async fn test_output_reader_does_not_wait_for_delayed_fragment() {
    let (reader, mut writer) = tokio::io::duplex(8);
    let writer = tokio::spawn(async move {
        writer.write_all(b"a").await.unwrap();
        tokio::time::sleep(Duration::from_millis(25)).await;
        writer.write_all(b"b").await.unwrap();
    });
    let mut output = OutputReader::new(reader, 8).unwrap();

    let first = tokio::time::timeout(Duration::from_millis(15), output.read_chunk())
        .await
        .expect("partial chunk did not flush promptly")
        .unwrap()
        .unwrap();
    assert_eq!(&first[..], b"a");
    assert_eq!(&output.read_chunk().await.unwrap().unwrap()[..], b"b");
    writer.await.unwrap();
}

#[test]
fn test_output_reader_rejects_zero_mtu() {
    assert!(OutputReader::new(tokio::io::empty(), 0).is_err());
}
