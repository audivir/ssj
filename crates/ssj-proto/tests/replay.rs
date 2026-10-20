use ssj_proto::{ReplayBuffer, ReplayError};

#[test]
fn full_buffer_takes_partial_writes_until_acknowledged() {
    let mut buf = ReplayBuffer::new(8);
    assert_eq!(buf.push(b"abcdef"), 6);
    assert_eq!(buf.push(b"ghijk"), 2);
    assert_eq!(buf.push(b"l"), 0);
    assert_eq!((buf.start(), buf.end(), buf.free()), (0, 8, 0));

    buf.ack(5).unwrap();
    assert_eq!((buf.start(), buf.end(), buf.free()), (5, 8, 5));
    assert_eq!(buf.push(b"ijklm"), 5);
    assert_eq!(buf.read_from(5, 100).unwrap(), b"fghijklm");
}

// many fill and drain cycles move the data around inside the ring.
#[test]
fn content_stays_correct_over_many_wraparounds() {
    let mut buf = ReplayBuffer::new(13);
    let stream: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
    let (mut written, mut acked) = (0usize, 0usize);
    while acked < stream.len() {
        written += buf.push(&stream[written..(written + 7).min(stream.len())]);
        let step = 5.min(written - acked);
        let seen = buf.read_from(acked as u64, step).unwrap();
        assert_eq!(seen, &stream[acked..acked + step]);
        acked += step;
        buf.ack(acked as u64).unwrap();
    }
}

#[test]
fn acknowledging_unsent_bytes_is_refused_and_changes_nothing() {
    let mut buf = ReplayBuffer::new(8);
    buf.push(b"abc");
    assert_eq!(buf.ack(4), Err(ReplayError::Ahead));
    assert_eq!((buf.start(), buf.end()), (0, 3));
    assert_eq!(buf.read_from(0, 10).unwrap(), b"abc");
}

#[test]
fn stale_acknowledgement_is_ignored() {
    let mut buf = ReplayBuffer::new(8);
    buf.push(b"abcdef");
    buf.ack(4).unwrap();
    buf.ack(2).unwrap();
    assert_eq!(buf.start(), 4);
    assert_eq!(buf.read_from(4, 10).unwrap(), b"ef");
}

#[test]
fn reading_outside_the_retained_range_reports_why() {
    let mut buf = ReplayBuffer::new(8);
    buf.push(b"abcdef");
    buf.ack(3).unwrap();
    assert_eq!(buf.read_from(2, 1), Err(ReplayError::Gone));
    assert_eq!(buf.read_from(7, 1), Err(ReplayError::Ahead));
    assert_eq!(buf.read_from(6, 1).unwrap(), b"");
    assert_eq!(buf.read_from(3, 2).unwrap(), b"de");
}

#[test]
fn tail_offset_never_points_before_the_retained_bytes() {
    let mut buf = ReplayBuffer::new(100);
    buf.push(&[0; 50]);
    assert_eq!(buf.tail_offset(20), 30);
    assert_eq!(buf.tail_offset(500), 0);
    buf.ack(40).unwrap();
    assert_eq!(buf.tail_offset(500), 40);
}

#[test]
fn evicting_push_forgets_the_oldest_bytes_and_reports_how_many() {
    let mut buf = ReplayBuffer::new(8);
    assert_eq!(buf.push_evicting(b"abcdef"), 0);
    assert_eq!((buf.start(), buf.end()), (0, 6));

    // two bytes of the old content go.
    assert_eq!(buf.push_evicting(b"ghij"), 2);
    assert_eq!((buf.start(), buf.end()), (2, 10));
    assert_eq!(buf.read_from(2, 100).unwrap(), b"cdefghij");

    // bytes before the start are gone, with the right reason.
    assert_eq!(buf.read_from(1, 1), Err(ReplayError::Gone));
}

#[test]
fn evicting_push_of_more_than_the_capacity_keeps_the_newest_bytes() {
    let mut buf = ReplayBuffer::new(4);
    buf.push(b"ab");
    // all old bytes and the first six new ones are lost.
    assert_eq!(buf.push_evicting(b"0123456789"), 8);
    assert_eq!((buf.start(), buf.end()), (8, 12));
    assert_eq!(buf.read_from(8, 100).unwrap(), b"6789");
}

/// xorshift, so that the test needs no dependency and is reproducible.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

// whatever mix of the three ways to add bytes is used, the buffer holds exactly the newest bytes
// that fit and agrees with a plain vector about every offset.
#[test]
fn evicting_and_waiting_pushes_agree_with_a_plain_vector() {
    let mut rng = Rng(0x2545f4914f6cdd1d);
    let capacity = 37;
    let mut buf = ReplayBuffer::new(capacity);
    let mut model: Vec<u8> = Vec::new();
    let mut start = 0usize;
    for round in 0..20_000u32 {
        let chunk: Vec<u8> = (0..rng.next() % 60)
            .map(|i| (round as u8).wrapping_add(i as u8))
            .collect();
        match rng.next() % 3 {
            0 => {
                let taken = buf.push(&chunk);
                assert_eq!(taken, chunk.len().min(capacity - (model.len() - start)));
                model.extend_from_slice(&chunk[..taken]);
            }
            1 => {
                let moved = buf.push_evicting(&chunk);
                model.extend_from_slice(&chunk);
                let new_start = start.max(model.len().saturating_sub(capacity));
                assert_eq!(moved, (new_start - start) as u64);
                start = new_start;
            }
            _ => {
                let offset = start + (rng.next() as usize) % (model.len() - start + 1);
                buf.ack(offset as u64).unwrap();
                start = offset;
            }
        }
        assert_eq!((buf.start(), buf.end()), (start as u64, model.len() as u64));
        assert_eq!(buf.read_from(start as u64, 1000).unwrap(), &model[start..]);
    }
}

// A read that starts or ends on either side of the point where the ring wraps is copied from two
// slices. Every start and length is compared with a plain model.
#[test]
fn reads_across_the_wrap_of_the_ring_match_a_plain_model() {
    let mut buf = ReplayBuffer::new(16);
    let mut model: Vec<u8> = Vec::new();
    let mut base = 0u64;
    let mut next = 0u8;
    for round in 0..40 {
        let fresh: Vec<u8> = (0..(round % 7 + 3))
            .map(|_| {
                next = next.wrapping_add(1);
                next
            })
            .collect();
        let stored = buf.push(&fresh);
        model.extend_from_slice(&fresh[..stored]);
        let drop = (round * 3 % 11).min(model.len());
        buf.ack(base + drop as u64).unwrap();
        model.drain(..drop);
        base += drop as u64;
        for from in 0..=model.len() {
            for max in [0, 1, 3, 7, 16, 100] {
                let got = buf.read_from(base + from as u64, max).unwrap();
                let want = &model[from..(from + max).min(model.len())];
                assert_eq!(got, want, "round {round} from {from} max {max}");
            }
        }
    }
}
