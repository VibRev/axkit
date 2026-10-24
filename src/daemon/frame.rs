//! The wire format between a client and its daemon: `[tag][u32 LE length][body]`.

use std::io::{self, ErrorKind, Read, Write};

/// Bumped on any change to frames or request fields; part of the daemon's
/// identity, so a client never meets a daemon speaking another version.
pub(super) const PROTOCOL: u32 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Tag {
    /// Client: run a command (the fields of a [`super::Request`]).
    Run,
    /// Client: report status.
    Status,
    /// Client: exit; body `[1]` forces it even while sessions are open.
    Quit,
    /// Daemon: a chunk of stdout.
    Out,
    /// Daemon: a chunk of stderr.
    Err,
    /// Daemon: a progress line, for the client's stderr.
    Progress,
    /// Daemon: the exit status (one byte); the reply ends.
    Exit,
    /// Daemon: not run here, run it yourself; body says why.
    Decline,
    /// Daemon: the status text.
    StatusReply,
}

impl Tag {
    fn byte(self) -> u8 {
        match self {
            Tag::Run => b'R',
            Tag::Status => b'S',
            Tag::Quit => b'Q',
            Tag::Out => b'O',
            Tag::Err => b'E',
            Tag::Progress => b'P',
            Tag::Exit => b'X',
            Tag::Decline => b'D',
            Tag::StatusReply => b'T',
        }
    }

    fn from_byte(b: u8) -> Option<Tag> {
        Some(match b {
            b'R' => Tag::Run,
            b'S' => Tag::Status,
            b'Q' => Tag::Quit,
            b'O' => Tag::Out,
            b'E' => Tag::Err,
            b'P' => Tag::Progress,
            b'X' => Tag::Exit,
            b'D' => Tag::Decline,
            b'T' => Tag::StatusReply,
            _ => return None,
        })
    }
}

pub(super) fn write_frame(w: &mut impl Write, tag: Tag, body: &[u8]) -> io::Result<()> {
    let len = u32::try_from(body.len()).map_err(|_| io::Error::from(ErrorKind::InvalidInput))?;
    let mut frame = Vec::with_capacity(5 + body.len());
    frame.push(tag.byte());
    frame.extend_from_slice(&len.to_le_bytes());
    frame.extend_from_slice(body);
    w.write_all(&frame)
}

/// The next frame, or `None` at a clean end of stream.
pub(super) fn read_frame(r: &mut impl Read) -> io::Result<Option<(Tag, Vec<u8>)>> {
    let mut head = [0u8; 5];
    match r.read_exact(&mut head) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let tag = Tag::from_byte(head[0]).ok_or_else(|| io::Error::new(ErrorKind::InvalidData, "unknown frame tag"))?;
    let len =
        usize::try_from(u32::from_le_bytes([head[1], head[2], head[3], head[4]])).map_err(|_| io::Error::from(ErrorKind::InvalidData))?;
    let mut body = vec![0; len];
    r.read_exact(&mut body)?;
    Ok(Some((tag, body)))
}

/// Append one length-prefixed field.
pub(super) fn put(buf: &mut Vec<u8>, bytes: &[u8]) {
    let len = u32::try_from(bytes.len()).expect("a request field is under 4 GiB");
    buf.extend_from_slice(&len.to_le_bytes());
    buf.extend_from_slice(bytes);
}

/// The length-prefixed fields of a body; `None` if one is cut short.
pub(super) fn fields(mut body: &[u8]) -> Option<Vec<&[u8]>> {
    let mut out = Vec::new();
    while !body.is_empty() {
        let len = usize::try_from(u32::from_le_bytes(body.get(..4)?.try_into().ok()?)).ok()?;
        out.push(body.get(4..4 + len)?);
        body = &body[4 + len..];
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_and_end_cleanly() {
        let mut buf = Vec::new();
        write_frame(&mut buf, Tag::Out, b"hello").unwrap();
        write_frame(&mut buf, Tag::Exit, &[2]).unwrap();
        let mut r = &buf[..];
        assert_eq!(read_frame(&mut r).unwrap(), Some((Tag::Out, b"hello".to_vec())));
        assert_eq!(read_frame(&mut r).unwrap(), Some((Tag::Exit, vec![2])));
        assert_eq!(read_frame(&mut r).unwrap(), None);
    }

    #[test]
    fn an_unknown_tag_is_refused() {
        let mut r = &[b'?', 0, 0, 0, 0][..];
        assert_eq!(read_frame(&mut r).unwrap_err().kind(), ErrorKind::InvalidData);
    }

    #[test]
    fn every_tag_round_trips_through_its_byte() {
        let all = [Tag::Run, Tag::Status, Tag::Quit, Tag::Out, Tag::Err, Tag::Progress, Tag::Exit, Tag::Decline, Tag::StatusReply];
        for tag in all {
            assert_eq!(Tag::from_byte(tag.byte()), Some(tag));
        }
    }

    #[test]
    fn fields_round_trip() {
        let mut req = Vec::new();
        for f in [&b"tag"[..], b"", b"/a b/c"] {
            put(&mut req, f);
        }
        assert_eq!(fields(&req).unwrap(), vec![&b"tag"[..], b"", b"/a b/c"]);
        assert!(fields(&req[..req.len() - 1]).is_none(), "a truncated field is refused");
    }
}
