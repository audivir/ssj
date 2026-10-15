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

// Many fill and drain cycles move the data around inside the ring.
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
