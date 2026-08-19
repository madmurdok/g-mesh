//! The shim<->daemon channel, named once and implemented per platform.
//!
//! Everything above this module talks about an [`Endpoint`] (where a
//! project's daemon can be reached), a blocking [`Stream`] (what the shim and
//! the CLI hold), a synchronously bound [`Listener`] (what `daemon::run`
//! creates before it writes its pid file) and the tokio pair the accept loop
//! turns that into ([`AsyncListener`], [`AsyncStream`]). Nothing above this
//! module names `AF_UNIX` or a named pipe.
//!
//! # Why hand-rolled rather than the `interprocess` crate
//!
//! `interprocess`'s local sockets are exactly this shape on paper - AF_UNIX
//! on POSIX, named pipes on Windows, with both a blocking and a tokio API -
//! and it was the first candidate. Three properties of *this* daemon rule it
//! out, all of them checked against interprocess 2.4.3's source rather than
//! guessed:
//!
//! 1. **No half-close.** `shim::proxy` answers EOF on its client's stdin with
//!    `shutdown(Shutdown::Write)`, so the daemon sees the request stream end
//!    and can still flush the replies it owes before closing. interprocess's
//!    sync `Stream` trait has no shutdown at all, and says so about the one
//!    thing that looks like it: "Dropping a half does not shut it down like
//!    it does with sockets". Adopting it would cost the Unix build a
//!    behaviour it has today.
//! 2. **No sync->async handoff.** `ListenerOptions` offers `create_sync` and
//!    `create_tokio` and nothing in between - no `from_sync`, no `from_std`.
//!    `create_tokio` needs a live tokio reactor, which would force the
//!    daemon's bind *inside* `serve_forever`'s runtime and move it after the
//!    pid file. `daemon::run` binds first, on the calling thread, on purpose:
//!    the shim's bootstrap budget is a race against the socket becoming
//!    connectable, and "the pid file exists" is only allowed to mean
//!    "something is listening" because the bind happened first.
//! 3. **It would replace the Unix path, not extend it.** The Unix transport
//!    is the measured, shipped one; routing it through a third-party wrapper
//!    to gain a platform is the trade this port is explicitly not allowed to
//!    make. Nineteen new packages, in a crate whose every dependency carries
//!    a paragraph saying why it is there, is the smaller half of that
//!    objection.
//!
//! So: [`unix`] is type aliases and one-line delegations over exactly the
//! `std::os::unix::net` and `tokio::net` types the daemon used before this
//! module existed - the Unix build compiles to the same code it always did -
//! and [`windows`] is a named-pipe implementation of the same six items.

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::{AsyncListener, AsyncStream, Endpoint, Listener, Stream};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{AsyncListener, AsyncStream, Endpoint, Listener, Stream};

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::Shutdown;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A fresh endpoint nothing else in this process (or on this machine) is
    /// using. On Unix that is a file in a temporary directory; on Windows the
    /// pipe namespace is global and has no temporary directory of its own, so
    /// uniqueness has to come from the name - hence the pid, which is also
    /// what keeps two concurrent `cargo test` runs apart there.
    fn unique_endpoint() -> (Option<tempfile::TempDir>, Endpoint) {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let id = format!("test-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed));

        #[cfg(unix)]
        {
            let dir = tempfile::tempdir().expect("failed to create a temporary directory");
            let endpoint = Endpoint::at_path(dir.path().join(format!("{id}.sock")));
            (Some(dir), endpoint)
        }
        #[cfg(windows)]
        {
            (None, Endpoint::named(&id))
        }
    }

    /// Accepts one connection and echoes whatever it is sent back, on a
    /// runtime of its own - the same arrangement `daemon::serve_forever` uses,
    /// and the reason the listener has to be bindable before any runtime
    /// exists.
    fn echo_once(listener: Listener) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to build the test runtime");
            runtime.block_on(async move {
                let mut listener = listener.into_async().expect("failed to go async");
                let mut stream = listener.accept().await.expect("failed to accept");
                let mut buf = [0u8; 512];
                loop {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    match stream.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(read) => {
                            if stream.write_all(&buf[..read]).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            });
        })
    }

    /// The whole transport in one pass: bind synchronously, hand the listener
    /// to tokio, connect a blocking client, and get bytes back across the
    /// same platform boundary the shim and the daemon sit on either side of.
    #[test]
    fn a_blocking_client_talks_to_an_async_listener() {
        let (_dir, endpoint) = unique_endpoint();
        let listener = Listener::bind(&endpoint).expect("failed to bind");
        let server = echo_once(listener);

        let stream = Stream::connect(&endpoint).expect("failed to connect");
        let mut writer = stream.try_clone().expect("failed to clone the connection");
        let mut reader = BufReader::new(stream);

        writer.write_all(b"hello\n").expect("failed to write");
        writer.flush().expect("failed to flush");

        let mut line = String::new();
        reader.read_line(&mut line).expect("failed to read");
        assert_eq!(line, "hello\n");

        // Reading on this thread while another writes is the arrangement
        // `shim::proxy` depends on, so it is asserted rather than assumed.
        writer.write_all(b"again\n").expect("failed to write a second time");
        let mut second = String::new();
        reader.read_line(&mut second).expect("failed to read a second time");
        assert_eq!(second, "again\n");

        writer.shutdown(Shutdown::Both).expect("failed to shut down");
        drop(writer);
        drop(reader);
        server.join().expect("the echo server panicked");
    }

    /// Nothing is listening, so connecting fails rather than hanging - the
    /// judgement `shim::incumbent` turns into `Incumbent::Absent` and
    /// `daemon::is_listening` into `false`.
    #[test]
    fn connecting_to_an_unbound_endpoint_fails_promptly() {
        let (_dir, endpoint) = unique_endpoint();
        let started = std::time::Instant::now();
        assert!(Stream::connect(&endpoint).is_err());
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "connecting to nothing must fail promptly, took {:?}",
            started.elapsed()
        );
    }

    /// A second listener on a name a live one already holds is refused. On
    /// Unix that is `AddrInUse` from `bind()`; on Windows it is
    /// `FILE_FLAG_FIRST_PIPE_INSTANCE` refusing to join someone else's pipe.
    /// Either way it is the check that stops two daemons serving one project
    /// if the singleton lock ever failed to.
    #[test]
    fn a_name_a_live_listener_holds_cannot_be_bound_twice() {
        let (_dir, endpoint) = unique_endpoint();
        let _first = Listener::bind(&endpoint).expect("failed to bind");
        assert!(Listener::bind(&endpoint).is_err());
    }

    /// `clear_stale` is what `daemon::run` calls before binding. On Unix it
    /// unlinks a socket file a dead daemon left behind, which is what makes
    /// the rebind below possible; on Windows there is nothing to unlink and
    /// the rebind works because the name went with the process.
    #[test]
    fn an_endpoint_can_be_rebound_after_its_listener_is_gone() {
        let (_dir, endpoint) = unique_endpoint();
        drop(Listener::bind(&endpoint).expect("failed to bind"));
        endpoint.clear_stale();
        Listener::bind(&endpoint).expect("failed to rebind a released endpoint");
    }
}
