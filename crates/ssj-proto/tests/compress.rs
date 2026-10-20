use ssj_proto::compress::{Compressor, InflateError, inflate};
use ssj_proto::frame::MAX_DATA_LEN;

fn text(len: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0u32;
    while out.len() < len {
        out.extend_from_slice(
            format!(
                "2026-10-18 build step {i}: compiling crate-{} ... ok\n",
                i % 97
            )
            .as_bytes(),
        );
        i += 1;
    }
    out.truncate(len);
    out
}

#[test]
fn text_shrinks_and_comes_back_exactly() {
    let mut compressor = Compressor::new();
    for len in [4096, 10_000, MAX_DATA_LEN] {
        let data = text(len);
        let packed = compressor.compress(&data).expect("text compresses");
        assert!(
            packed.len() < data.len() / 3,
            "{} of {}",
            packed.len(),
            data.len()
        );
        assert_eq!(inflate(&packed).unwrap(), data);
    }
}

// the same compressor is used for every frame, and a frame must not depend on the one before.
#[test]
fn frames_are_independent_of_each_other() {
    let mut compressor = Compressor::new();
    let first = compressor.compress(&text(20_000)).unwrap();
    let second = compressor.compress(&text(20_000)).unwrap();
    assert_eq!(first, second);
    // and the second can be read without the first.
    assert_eq!(inflate(&second).unwrap(), text(20_000));
}

#[test]
fn data_that_does_not_shrink_is_left_alone() {
    let mut state = 0x1234_5678_u32;
    let noise: Vec<u8> = (0..20_000)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state as u8
        })
        .collect();
    assert!(Compressor::new().compress(&noise).is_none());
    assert!(Compressor::new().compress(&[]).is_none());
    assert!(
        Compressor::new()
            .compress(&vec![0; MAX_DATA_LEN + 1])
            .is_none()
    );
}

#[test]
fn bad_input_is_an_error_and_a_bomb_is_refused() {
    let mut compressor = Compressor::new();
    let packed = compressor.compress(&text(30_000)).unwrap();
    // cut off
    assert_eq!(
        inflate(&packed[..packed.len() / 2]),
        Err(InflateError::Invalid)
    );
    assert!(inflate(&[]).is_err());
    assert!(inflate(&[0xff; 50]).is_err());
    let mut damaged = packed.clone();
    let middle = damaged.len() / 2;
    damaged[middle] ^= 0xff;
    // damage either breaks the frame or changes the data, but never panics.
    let _ = inflate(&damaged);

    // four megabytes of zeros are a few bytes of zstd: the header says so, and it is refused
    // before anything is allocated for the content.
    let bomb = zstd::bulk::compress(&vec![0u8; 4 << 20], 19).unwrap();
    assert!(bomb.len() < 1000, "{}", bomb.len());
    assert_eq!(inflate(&bomb), Err(InflateError::TooLarge));

    // a frame that does not say how large it is cannot be checked first, so it is refused.
    let mut unsized_frame = Vec::new();
    let mut encoder = zstd::stream::write::Encoder::new(&mut unsized_frame, 3).unwrap();
    std::io::Write::write_all(&mut encoder, &vec![0u8; 100_000]).unwrap();
    encoder.finish().unwrap();
    assert_eq!(inflate(&unsized_frame), Err(InflateError::Invalid));

    // a frame of exactly the largest size is fine.
    let largest = vec![b'a'; MAX_DATA_LEN];
    let packed = zstd::bulk::compress(&largest, 3).unwrap();
    assert_eq!(inflate(&packed).unwrap(), largest);
    let too_big = zstd::bulk::compress(&vec![b'a'; MAX_DATA_LEN + 1], 3).unwrap();
    assert_eq!(inflate(&too_big), Err(InflateError::TooLarge));
}
