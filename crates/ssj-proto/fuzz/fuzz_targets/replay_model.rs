// fuzz target. The first line is a comment, so that the file does not look like a script.

#![no_main]

use libfuzzer_sys::fuzz_target;
use ssj_proto::{ReplayBuffer, ReplayError};

// runs random operations against the buffer and a plain `Vec` that holds all bytes ever written.
fuzz_target!(|ops: &[u8]| {
    const CAPACITY: usize = 64;
    let mut buf = ReplayBuffer::new(CAPACITY);
    let (mut model, mut start) = (Vec::<u8>::new(), 0usize);
    let mut counter = 0u8;

    for op in ops.chunks(2) {
        let arg = usize::from(op.get(1).copied().unwrap_or(0));
        match op[0] % 4 {
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
            3 => {
                let chunk: Vec<u8> = (0..arg % 90)
                    .map(|_| {
                        counter = counter.wrapping_add(1);
                        counter
                    })
                    .collect();
                let moved = buf.push_evicting(&chunk);
                model.extend_from_slice(&chunk);
                let new_start = start.max(model.len().saturating_sub(CAPACITY));
                assert_eq!(moved, (new_start - start) as u64);
                start = new_start;
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
