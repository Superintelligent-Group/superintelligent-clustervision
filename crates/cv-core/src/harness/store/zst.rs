//! The one zstd decoder for compressed sources (Codex rollouts, Zed and OpenClaw blobs). Native
//! builds use libzstd, already linked into the shipped binary through tantivy and about 4x
//! ruzstd's decode rate (`show` of a 412 MB rollout: 28 G -> 14 G instructions); wasm keeps ruzstd.
//! Both decode exactly one frame, streaming, with a bounded window.
use std::io::{self, Read};

/// A reader over the first zstd frame of `input`. A malformed header fails here, as ruzstd's
/// constructor did, so callers keep telling "not zstd" apart from a failure mid-stream.
#[cfg(not(target_family = "wasm"))]
pub(crate) fn decoder<R: Read>(input: R) -> io::Result<impl Read> {
    use std::io::BufRead;
    let mut decoded = io::BufReader::with_capacity(64 * 1024, zstd::stream::read::Decoder::new(input)?.single_frame());
    decoded.fill_buf()?;
    Ok(decoded)
}

#[cfg(target_family = "wasm")]
pub(crate) fn decoder<R: Read>(input: R) -> io::Result<impl Read> {
    ruzstd::decoding::StreamingDecoder::new(input).map_err(io::Error::other)
}
