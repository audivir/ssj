use ssj_proto::resume::{ATTACH_TAIL, Action, ServerView, check_welcome, negotiate};
use ssj_proto::{Env, ErrorCode, Hello, ReplayBuffer, Welcome, WindowSize};

fn size() -> WindowSize {
    WindowSize::new(24, 80).unwrap()
}

/// Output buffer that has produced `end` bytes of which everything before `start` is acknowledged.
fn output(start: u64, end: u64) -> ReplayBuffer {
    let mut buf = ReplayBuffer::new(1 << 20);
    buf.push(&vec![0; end as usize]);
    buf.ack(start).unwrap();
    buf
}

fn view(shell_started: bool, output: &ReplayBuffer, input_received: u64) -> ServerView<'_> {
    ServerView {
        shell_started,
        output,
        input_received,
    }
}

#[test]
fn start_creates_the_shell_only_once() {
    let out = output(0, 0);
    let hello = Hello::Start {
        size: size(),
        env: Env::default(),
    };

    let (welcome, action) = negotiate(hello.clone(), &view(false, &out, 0)).unwrap();
    assert_eq!(
        welcome,
        Welcome {
            input_offset: 0,
            output_offset: 0
        }
    );
    assert_eq!(
        action,
        Action::Spawn {
            size: size(),
            env: Env::default()
        }
    );

    assert_eq!(
        negotiate(hello, &view(true, &out, 0)).unwrap_err(),
        ErrorCode::AlreadyStarted
    );
}

#[test]
fn resume_and_attach_need_a_running_shell() {
    let out = output(0, 0);
    for hello in [
        Hello::Resume {
            size: size(),
            output_offset: 0,
        },
        Hello::Attach { size: size() },
    ] {
        assert_eq!(
            negotiate(hello, &view(false, &out, 0)).unwrap_err(),
            ErrorCode::NotStarted
        );
    }
}

#[test]
fn resume_accepts_exactly_the_retained_offsets() {
    let out = output(100, 200);
    let resume = |output_offset| Hello::Resume {
        size: size(),
        output_offset,
    };

    for offset in [100, 150, 200] {
        let (welcome, action) = negotiate(resume(offset), &view(true, &out, 42)).unwrap();
        assert_eq!(
            welcome,
            Welcome {
                input_offset: 42,
                output_offset: offset
            }
        );
        assert_eq!(action, Action::Resize(size()));
    }
    for offset in [0, 99, 201, u64::MAX] {
        assert_eq!(
            negotiate(resume(offset), &view(true, &out, 42)).unwrap_err(),
            ErrorCode::ReplayGap,
            "{offset}"
        );
    }
}

#[test]
fn attach_replays_at_most_the_tail_of_the_output() {
    let hello = Hello::Attach { size: size() };

    let long = output(0, ATTACH_TAIL as u64 * 3);
    let (welcome, _) = negotiate(hello.clone(), &view(true, &long, 5)).unwrap();
    assert_eq!(
        welcome,
        Welcome {
            input_offset: 5,
            output_offset: ATTACH_TAIL as u64 * 2
        }
    );

    // acknowledged output is gone, so the tail cannot start before the retained bytes
    let short = output(900, 1000);
    let (welcome, _) = negotiate(hello, &view(true, &short, 0)).unwrap();
    assert_eq!(welcome.output_offset, 900);
}

#[test]
fn client_refuses_a_welcome_that_contradicts_its_own_state() {
    let resume = Hello::Resume {
        size: size(),
        output_offset: 500,
    };
    let welcome = |input_offset, output_offset| Welcome {
        input_offset,
        output_offset,
    };
    // the client sent 30 input bytes and got 10 acknowledged
    let check = |w| check_welcome(&resume, w, 10, 30).is_ok();

    assert!(check(welcome(10, 500)));
    assert!(check(welcome(30, 500)));
    assert!(!check(welcome(31, 500)), "claims input that was never sent");
    assert!(!check(welcome(9, 500)), "takes back acknowledged input");
    assert!(!check(welcome(20, 501)), "moves the output offset");

    let start = Hello::Start {
        size: size(),
        env: Env::default(),
    };
    assert!(check_welcome(&start, welcome(0, 0), 0, 0).is_ok());
    assert!(check_welcome(&start, welcome(1, 0), 0, 0).is_err());

    // a client without history takes whatever the server says
    assert!(check_welcome(&Hello::Attach { size: size() }, welcome(999, 77), 0, 0).is_ok());
}
