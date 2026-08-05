//! Peer-credential verification for the client ↔ daemon Unix socket.
//!
//! The socket is the whole trust boundary: whoever is on the other end sees
//! every keystroke of every pane and can inject input into all of them. Mode
//! `0600` on the socket file is the first line of defence, but it is not the
//! only one — `$MULT_SOCKET_PATH` can point anywhere, including a directory
//! another user can write, so both sides ask the kernel who the peer actually
//! is instead of trusting the path.
//!
//! This lives in the protocol crate because the client and the daemon must
//! perform *the same* check: it used to be copy-pasted into both binaries, each
//! with its own tests, so hardening one left the other open.
//!
//! **Fail closed.** A platform where the kernel cannot tell us the peer's uid
//! gets an error, not a pass. The previous implementation returned "unknown" on
//! every non-Linux target and both callers read that as "accept", which meant
//! macOS and the BSDs had no peer check at all while the docs claimed one.

use std::{io, os::unix::net::UnixStream};

/// The effective uid of this process.
///
/// The single definition; it used to exist in four places.
pub fn effective_uid() -> u32 {
    // SAFETY: `geteuid` is always successful and has no failure mode.
    unsafe { libc::geteuid() as u32 }
}

/// Reject `stream` unless its peer runs as this process's effective uid.
///
/// `peer_label` names the peer in the error ("client", "mult-server") so the
/// message says which side was refused.
pub fn verify_peer_is_self(stream: &UnixStream, peer_label: &str) -> io::Result<()> {
    let peer_uid = peer_uid(stream).map_err(|error| {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("refusing {peer_label}: cannot determine peer uid: {error}"),
        )
    })?;
    verify_peer_uid(peer_uid, effective_uid(), peer_label)
}

/// Pure core of [`verify_peer_is_self`], so the accept/reject rule is testable
/// without a peer running under a different uid.
fn verify_peer_uid(peer_uid: u32, current_uid: u32, peer_label: &str) -> io::Result<()> {
    if peer_uid == current_uid {
        return Ok(());
    }

    Err(io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("rejecting {peer_label} uid {peer_uid}; expected current uid {current_uid}"),
    ))
}

/// The peer's uid, or an error when the kernel will not tell us.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    use std::os::fd::AsRawFd;

    let mut credentials = std::mem::MaybeUninit::<libc::ucred>::uninit();
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: the fd is owned by `stream` and outlives the call; the buffer and
    // its length describe a `libc::ucred` the kernel fills in.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            credentials.as_mut_ptr().cast(),
            &mut length,
        )
    };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    if length < std::mem::size_of::<libc::ucred>() as libc::socklen_t {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "short SO_PEERCRED response",
        ));
    }
    // SAFETY: `getsockopt` succeeded and filled at least `size_of::<ucred>()`.
    Ok(unsafe { credentials.assume_init().uid })
}

/// macOS and the BSDs have no `SO_PEERCRED`. `getpeereid(3)` is the portable
/// spelling of the same question; on macOS it is implemented on top of
/// `LOCAL_PEERCRED`/`struct xucred`, so this is that check, not a weaker one.
#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "netbsd",
    target_os = "openbsd"
))]
fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    use std::os::fd::AsRawFd;

    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    // SAFETY: the fd is owned by `stream` and outlives the call; both out
    // parameters are valid, correctly typed and initialised.
    let result = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(uid as u32)
}

/// Any other Unix: no known way to ask, so the connection is refused. Silently
/// accepting would leave a socket that anyone who can reach the path may drive.
#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "netbsd",
    target_os = "openbsd"
)))]
fn peer_uid(_stream: &UnixStream) -> io::Result<u32> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "peer credentials are not available on this platform",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_socket_pair_peer_is_this_user() {
        let (client, _server) = UnixStream::pair().expect("create socket pair");

        // Both ends belong to this process, so the check must succeed — and, on
        // every supported platform, must actually have consulted the kernel
        // rather than returned "unknown".
        verify_peer_is_self(&client, "test peer").expect("same uid peer is accepted");
        assert_eq!(peer_uid(&client).expect("peer uid"), effective_uid());
    }

    #[test]
    fn a_foreign_uid_is_rejected() {
        let current = effective_uid();

        let error = verify_peer_uid(current.wrapping_add(1), current, "test peer")
            .expect_err("a different uid must be rejected");

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("rejecting test peer"));
    }

    #[test]
    fn effective_uid_matches_geteuid() {
        assert_eq!(effective_uid(), unsafe { libc::geteuid() } as u32);
    }
}
