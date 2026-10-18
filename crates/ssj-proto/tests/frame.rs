mod common;

use common::{client_frames, server_frames};
use ssj_proto::frame::{MAX_DATA_LEN, MAX_FRAME_LEN};
use ssj_proto::{ClientFrame, DecodeError, EnvError, ServerFrame, decode_client, decode_server};

/// Feeds `stream` in chunks of `step` bytes, like a socket would, and collects all frames.
type Decode<F> = fn(&[u8]) -> Result<Option<(F, usize)>, DecodeError>;

fn decode_in_chunks<F>(stream: &[u8], step: usize, decode: Decode<F>) -> Vec<F> {
    let (mut buf, mut frames) = (Vec::new(), Vec::new());
    for chunk in stream.chunks(step) {
        buf.extend_from_slice(chunk);
        while let Some((frame, used)) = decode(&buf).unwrap() {
            frames.push(frame);
            buf.drain(..used);
        }
    }
    assert!(buf.is_empty(), "{} bytes left over", buf.len());
    frames
}

#[test]
fn client_frames_survive_any_chunking_of_the_stream() {
    let frames = client_frames();
    let mut stream = Vec::new();
    for frame in &frames {
        frame.encode(&mut stream).unwrap();
    }
    for step in [1, 2, 3, 5, 7, 1000, stream.len()] {
        assert_eq!(
            decode_in_chunks(&stream, step, decode_client),
            frames,
            "step {step}"
        );
    }
}

#[test]
fn server_frames_survive_any_chunking_of_the_stream() {
    let frames = server_frames();
    let mut stream = Vec::new();
    for frame in &frames {
        frame.encode(&mut stream).unwrap();
    }
    for step in [1, 2, 3, 5, 7, 1000, stream.len()] {
        assert_eq!(
            decode_in_chunks(&stream, step, decode_server),
            frames,
            "step {step}"
        );
    }
}

// The announced length is judged from the 4 header bytes, before any body is buffered.
#[test]
fn bad_length_is_rejected_from_the_header_alone() {
    for len in [0u32, MAX_FRAME_LEN as u32 + 1, u32::MAX] {
        let header = len.to_be_bytes();
        assert_eq!(
            decode_client(&header).unwrap_err(),
            DecodeError::BadLength,
            "{len}"
        );
        assert_eq!(
            decode_server(&header).unwrap_err(),
            DecodeError::BadLength,
            "{len}"
        );
    }
    // the largest allowed frame just waits for its body
    let header = (MAX_FRAME_LEN as u32).to_be_bytes();
    assert_eq!(decode_client(&header), Ok(None));
}

fn raw(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut out = ((body.len() + 1) as u32).to_be_bytes().to_vec();
    out.push(tag);
    out.extend_from_slice(body);
    out
}

#[test]
fn malformed_client_frames_are_rejected() {
    let size = [0, 24, 0, 80];
    let hello = |kind: u8, rest: &[u8]| raw(0x01, &[&[kind][..], &size, rest].concat());
    let cases: Vec<(&str, Vec<u8>, DecodeError)> = vec![
        (
            "unknown type",
            raw(0x7f, &[]),
            DecodeError::UnknownType(0x7f),
        ),
        (
            "server tag from a client",
            raw(0x81, &[0; 16]),
            DecodeError::UnknownType(0x81),
        ),
        (
            "close with a body",
            raw(0x05, &[0]),
            DecodeError::TrailingBytes,
        ),
        ("ack too short", raw(0x04, &[0; 7]), DecodeError::Truncated),
        (
            "ack too long",
            raw(0x04, &[0; 9]),
            DecodeError::TrailingBytes,
        ),
        ("empty input", raw(0x02, &[]), DecodeError::InvalidValue),
        (
            "oversized input",
            raw(0x02, &vec![0; MAX_DATA_LEN + 1]),
            DecodeError::InvalidValue,
        ),
        (
            "zero rows",
            raw(0x03, &[0, 0, 0, 80]),
            DecodeError::InvalidValue,
        ),
        (
            "zero cols",
            raw(0x03, &[0, 24, 0, 0]),
            DecodeError::InvalidValue,
        ),
        (
            "huge cols",
            raw(0x03, &[0, 24, 0xff, 0xff]),
            DecodeError::InvalidValue,
        ),
        (
            "unknown hello kind",
            hello(9, &[]),
            DecodeError::InvalidValue,
        ),
        (
            "attach with trailing bytes",
            hello(2, &[1]),
            DecodeError::TrailingBytes,
        ),
        (
            "resume without offset",
            hello(1, &[0; 4]),
            DecodeError::Truncated,
        ),
        // would allocate 65535 pairs if the count were trusted
        (
            "env count over the limit",
            hello(0, &[0xff, 0xff]),
            DecodeError::Env(EnvError::TooMany),
        ),
        (
            "env missing entries",
            hello(0, &[0, 2, 0, 1, b'A', 0, 1, b'B']),
            DecodeError::Truncated,
        ),
        (
            "env name is not utf-8",
            hello(0, &[0, 1, 0, 1, 0xff, 0, 0]),
            DecodeError::InvalidValue,
        ),
        (
            "env name with equals",
            hello(0, &[0, 1, 0, 3, b'A', b'=', b'B', 0, 0]),
            DecodeError::Env(EnvError::InvalidName),
        ),
        (
            "env value with NUL",
            hello(0, &[0, 1, 0, 1, b'A', 0, 1, 0]),
            DecodeError::Env(EnvError::InvalidValue),
        ),
        (
            "env duplicate",
            hello(0, &[0, 2, 0, 1, b'A', 0, 0, 0, 1, b'A', 0, 0]),
            DecodeError::Env(EnvError::Duplicate),
        ),
    ];
    for (name, bytes, expected) in cases {
        assert_eq!(decode_client(&bytes).unwrap_err(), expected, "{name}");
    }
}

#[test]
fn malformed_server_frames_are_rejected() {
    let cases: Vec<(&str, Vec<u8>, DecodeError)> = vec![
        (
            "client tag from a server",
            raw(0x05, &[]),
            DecodeError::UnknownType(0x05),
        ),
        (
            "welcome too short",
            raw(0x81, &[0; 15]),
            DecodeError::Truncated,
        ),
        ("empty output", raw(0x82, &[]), DecodeError::InvalidValue),
        (
            "unknown exit kind",
            raw(0x84, &[2, 0]),
            DecodeError::InvalidValue,
        ),
        (
            "unknown error code",
            raw(0x85, &[0]),
            DecodeError::InvalidValue,
        ),
        (
            "unknown error code, high",
            raw(0x85, &[200]),
            DecodeError::InvalidValue,
        ),
    ];
    for (name, bytes, expected) in cases {
        assert_eq!(decode_server(&bytes).unwrap_err(), expected, "{name}");
    }
}

#[test]
fn encoding_bad_data_leaves_the_buffer_unchanged() {
    for frame in [
        ClientFrame::Input(vec![]),
        ClientFrame::Input(vec![0; MAX_DATA_LEN + 1]),
    ] {
        let mut out = b"keep".to_vec();
        assert!(frame.encode(&mut out).is_err());
        assert_eq!(out, b"keep");
    }
    let mut out = b"keep".to_vec();
    assert!(ServerFrame::Output(vec![]).encode(&mut out).is_err());
    assert_eq!(out, b"keep");
}

/// xorshift, so the test needs no dependency and is reproducible.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

// Cheap stand-in for the real fuzzer, which runs on a single platform only.
#[test]
fn decoders_do_not_panic_on_random_or_mutated_input() {
    let mut rng = Rng(0x9e3779b97f4a7c15);
    let mut seeds = Vec::new();
    for frame in client_frames() {
        let mut bytes = Vec::new();
        frame.encode(&mut bytes).unwrap();
        seeds.push((bytes, true));
    }
    for frame in server_frames() {
        let mut bytes = Vec::new();
        frame.encode(&mut bytes).unwrap();
        seeds.push((bytes, false));
    }

    for round in 0..200_000u64 {
        let (mut bytes, _) = seeds[(rng.next() % seeds.len() as u64) as usize].clone();
        match round % 3 {
            // flip a few bits
            0 => {
                for _ in 0..1 + rng.next() % 4 {
                    let at = (rng.next() % bytes.len() as u64) as usize;
                    bytes[at] ^= 1 << (rng.next() % 8);
                }
            }
            // cut off
            1 => bytes.truncate((rng.next() % (bytes.len() as u64 + 1)) as usize),
            // pure noise behind a plausible header
            _ => {
                let len = (rng.next() % 64) as usize;
                bytes = (1 + len as u32).to_be_bytes().to_vec();
                bytes.extend((0..=len).map(|_| rng.next() as u8));
            }
        }
        for result in [
            decode_client(&bytes).map(|r| r.map(|(_, n)| n)),
            decode_server(&bytes).map(|r| r.map(|(_, n)| n)),
        ] {
            if let Ok(Some(used)) = result {
                assert!(used <= bytes.len());
            }
        }
    }
}

#[test]
fn ping_and_pong_carry_no_body() {
    assert_eq!(
        decode_client(&raw(0x07, &[0])).unwrap_err(),
        DecodeError::TrailingBytes
    );
    assert_eq!(
        decode_server(&raw(0x86, &[0])).unwrap_err(),
        DecodeError::TrailingBytes
    );
    assert_eq!(
        decode_client(&raw(0x07, &[])),
        Ok(Some((ClientFrame::Ping, 5)))
    );
    assert_eq!(
        decode_server(&raw(0x86, &[])),
        Ok(Some((ServerFrame::Pong, 5)))
    );
}

// Looking at a frame must agree with decoding it, for every frame and every cut of the stream.
#[test]
fn peeking_agrees_with_decoding() {
    use ssj_proto::{Peek, peek_server};

    for frame in server_frames() {
        let mut bytes = Vec::new();
        frame.encode(&mut bytes).unwrap();
        for cut in 0..bytes.len() {
            assert_eq!(
                peek_server(&bytes[..cut]),
                Ok(Peek::Incomplete),
                "{frame:?} cut {cut}"
            );
        }
        let (decoded, used) = decode_server(&bytes).unwrap().unwrap();
        match (peek_server(&bytes).unwrap(), &decoded) {
            (Peek::Output { data, used: peeked }, ServerFrame::Output(expected)) => {
                assert_eq!(&bytes[data], expected.as_slice());
                assert_eq!(peeked, used);
            }
            (Peek::Other, other) => assert!(!matches!(other, ServerFrame::Output(_))),
            (peeked, decoded) => panic!("{peeked:?} for {decoded:?}"),
        }
    }
    // a frame of one kind followed by another: only the first counts
    let mut two = Vec::new();
    ServerFrame::Pong.encode(&mut two).unwrap();
    ServerFrame::Output(b"x".to_vec()).encode(&mut two).unwrap();
    assert_eq!(peek_server(&two), Ok(Peek::Other));
    // an output frame without data, and a header that is too large, are errors
    assert!(peek_server(&[0, 0, 0, 1, 0x82]).is_err());
    assert!(peek_server(&[0xff, 0xff, 0xff, 0xff]).is_err());
}
