//! Peer credential verification for the per-user Unix socket.
//!
//! The socket is the daemon's whole authority boundary: whoever is on the other
//! end can read every keystroke of every pane and inject input into them. Mode
//! `0600` plus a private parent directory is the first barrier, but a squatted
//! socket path (another user's process listening where the client expects the
//! daemon) defeats file permissions entirely, so both ends check the peer's
//! effective UID before exchanging anything.
//!
//! This module is the *single* implementation of that check. It previously
//! existed byte-for-byte in both `src/pty.rs` and `src/bin/mult-server.rs`,
//! where it returned "credential unavailable" on every non-Linux target and
//! both callers read that as *accept* — a fail-open hole on macOS/BSD. The
//! contract here is deliberately narrower: either the kernel tells us who the
//! peer is and it is us, or the connection is refused.

use std::{io, os::unix::net::UnixStream};

/// The effective UID of this process.
///
/// The single source for "who are we" across both binaries: state/config
/// ownership checks, the `/tmp` socket fallback component, and this module's
/// peer comparison all resolve to this call. `libc::uid_t` is `u32` on every
/// platform this builds for, which the return type pins.
pub fn effective_uid() -> u32 {
    // SAFETY: `geteuid` takes no arguments, touches no memory, and cannot fail.
    unsafe { libc::geteuid() }
}

/// Reject the connection unless its peer runs as this effective user.
///
/// `label` names the peer in the error ("client", "mult-server") so a rejection
/// says which side was refused. An unobtainable credential is a hard failure:
/// an unverifiable peer is treated exactly like a hostile one.
pub fn verify_peer_is_self(stream: &UnixStream, label: &str) -> io::Result<()> {
    compare_peer_uid(peer_uid(stream)?, effective_uid(), label)
}

/// The effective UID of the process connected to `stream`.
///
/// Errors when the platform cannot report it, which is what makes the check
/// fail closed.
pub fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    platform_peer_uid(stream)
}

fn compare_peer_uid(peer: u32, current: u32, label: &str) -> io::Result<()> {
    if peer == current {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("rejecting {label} uid {peer}; expected current uid {current}"),
        ))
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn platform_peer_uid(stream: &UnixStream) -> io::Result<u32> {
    use std::os::fd::AsRawFd;

    let mut credentials = std::mem::MaybeUninit::<libc::ucred>::uninit();
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: the socket outlives the call, and `credentials`/`length` describe
    // one correctly sized, writable `ucred`.
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
    // SAFETY: a successful `getsockopt` of the full length initialized it.
    Ok(unsafe { credentials.assume_init() }.uid)
}

#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "watchos",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "openbsd",
    target_os = "netbsd",
))]
fn platform_peer_uid(stream: &UnixStream) -> io::Result<u32> {
    use std::os::fd::AsRawFd;

    // `getpeereid(3)` is the BSD/macOS equivalent of `SO_PEERCRED`: it reports
    // the credentials the peer had when it called `connect`, taken from the
    // kernel's socket state rather than from anything the peer can assert.
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    // SAFETY: the socket outlives the call and both out-parameters are valid,
    // correctly typed, writable locals.
    let result = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(uid)
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "watchos",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "openbsd",
    target_os = "netbsd",
)))]
fn platform_peer_uid(_stream: &UnixStream) -> io::Result<u32> {
    // Fail closed. A platform without a peer-credential API cannot distinguish
    // the daemon from a squatter, and silently accepting was the C3 defect.
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "this platform cannot report Unix socket peer credentials, so the peer cannot be verified",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_user_socket_pair_is_accepted() {
        let (client, _server) = UnixStream::pair().expect("socket pair");

        verify_peer_is_self(&client, "test peer").expect("same uid peer is accepted");
        assert_eq!(peer_uid(&client).expect("peer uid"), effective_uid());
    }

    #[test]
    fn a_different_uid_is_rejected_with_both_uids_named() {
        let error = compare_peer_uid(41, 42, "test peer").expect_err("different uid is rejected");

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("rejecting test peer uid 41"));
        assert!(error.to_string().contains("expected current uid 42"));
    }

    #[test]
    fn an_unverifiable_peer_is_a_hard_failure() {
        // The platform shim is what decides this, so assert the contract that
        // makes the check fail closed: `peer_uid` never reports "unknown".
        let (client, _server) = UnixStream::pair().expect("socket pair");
        let uid: io::Result<u32> = peer_uid(&client);

        assert!(uid.is_ok() || uid.unwrap_err().kind() == io::ErrorKind::Unsupported);
    }
}
