//! `g_mesh::log_line!` writes each line in one `write(2)`, so
//! processes sharing the daemon log never split each other's lines. The tests
//! themselves are in `log_line/shared.rs`, shared with the plugin SDK's copy
//! of the macro (`plugins/sdk/tests/log_line_atomic.rs`); the one below is
//! core's only, because it needs `libc` to grow a socket's buffers.
#![cfg(unix)]

use g_mesh::log_line;

include!("log_line/shared.rs");

/// Behaviour 1, the long case: a line over 64 KiB is still one `write(2)`.
/// The socket buffers are grown first, since a datagram socket refuses a
/// datagram bigger than its send buffer (2 KiB by default on macOS).
#[test]
fn a_line_over_64_kib_is_one_write() {
    fn grow(socket: &UnixDatagram, option: libc::c_int) {
        use std::os::fd::AsRawFd;
        let size: libc::c_int = 1 << 20;
        // SAFETY: a valid socket fd and a pointer to a c_int of the size passed.
        let rc = unsafe {
            libc::setsockopt(
                socket.as_raw_fd(),
                libc::SOL_SOCKET,
                option,
                (&size as *const libc::c_int).cast(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        assert_eq!(rc, 0, "setsockopt failed: {}", std::io::Error::last_os_error());
    }

    let datagrams = datagrams_from_child("probe-long", |child_end, reader| {
        grow(child_end, libc::SO_SNDBUF);
        grow(reader, libc::SO_RCVBUF);
    });
    let expected = format!("probe-long: len={LONG_PROBE_BODY} body={}\n", "y".repeat(LONG_PROBE_BODY));
    let lengths: Vec<usize> = datagrams.iter().map(Vec::len).collect();
    assert_eq!(lengths, [expected.len()], "expected the whole line in one write");
    assert_eq!(datagrams[0], expected.as_bytes());
}
