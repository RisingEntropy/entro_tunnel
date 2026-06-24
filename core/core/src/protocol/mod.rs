//! Application protocol: the [`Frame`] enum and its reader/writer that sit on
//! top of a transport [`MessageSink`]/[`MessageStream`].

pub mod control;
pub mod frame;

pub use control::{Hello, PeerInfo, TargetAddr, Welcome};
pub use frame::Frame;

use crate::transport::{BoxSink, BoxStream};
use crate::Result;

/// Writes [`Frame`]s onto a (already-encrypted) message sink.
pub struct FrameWriter {
    sink: BoxSink,
}

/// Reads [`Frame`]s from a (already-encrypted) message stream.
pub struct FrameReader {
    stream: BoxStream,
}

impl FrameWriter {
    pub fn new(sink: BoxSink) -> Self {
        Self { sink }
    }

    pub async fn send(&mut self, frame: &Frame) -> Result<()> {
        let bytes = bincode::serialize(frame)?;
        self.sink.send(&bytes).await
    }

    pub async fn close(&mut self) -> Result<()> {
        self.sink.close().await
    }
}

impl FrameReader {
    pub fn new(stream: BoxStream) -> Self {
        Self { stream }
    }

    pub async fn recv(&mut self) -> Result<Frame> {
        let bytes = self.stream.recv().await?;
        Ok(bincode::deserialize(&bytes)?)
    }
}

/// Split a connected channel into a frame writer/reader pair.
pub fn frames(sink: BoxSink, stream: BoxStream) -> (FrameWriter, FrameReader) {
    (FrameWriter::new(sink), FrameReader::new(stream))
}
