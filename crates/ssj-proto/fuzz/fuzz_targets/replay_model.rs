#![no_main]

use libfuzzer_sys::fuzz_target;
use ssj_proto::{ReplayBuffer, ReplayError};

// Runs random operations against the buffer and a plain `Vec` that holds all bytes ever written.
fuzz_target!(|ops: &[u8]| {
    const CAPACITY: usize = 64;
    let mut buf = ReplayBuffer::new(CAPACITY);
    let (mut model, mut start) = (Vec::<u8>::new(), 0usize);
    let mut counter = 0u8;

    for op in ops.chunks(2) {
        let arg = usize::from(op.get(1).copied().unwrap_or(0));
        match op[0] % 3 {
            0 => {
                let chunk: Vec<u8> = (0..arg % 40)
                    .map(|_| {
                        counter = counter.wrapping_add(1);
                        counter
                    })
                    .collect();
                let taken = buf.push(&chunk);
                assert_eq!(taken, chunk.len().min(CAPACITY - (model.len() - start)));
                model.extend_from_slice(&chunk[..taken]);
            }
            1 => {
                let offset = arg % (model.len() + 8);
                match buf.ack(offset as u64) {
                    Ok(()) => start = start.max(offset),
                    Err(err) => {
                        assert_eq!(err, ReplayError::Ahead);
                        assert!(offset > model.len());
                    }
                }
            }
            _ => {
                let offset = arg % (model.len() + 8);
                match buf.read_from(offset as u64, 16) {
                    Ok(bytes) => {
                        assert!((start..=model.len()).contains(&offset));
                        assert_eq!(bytes, &model[offset..(offset + 16).min(model.len())]);
                    }
                    Err(ReplayError::Gone) => assert!(offset < start),
                    Err(ReplayError::Ahead) => assert!(offset > model.len()),
                }
            }
        }
        assert_eq!((buf.start(), buf.end()), (start as u64, model.len() as u64));
    }
});
