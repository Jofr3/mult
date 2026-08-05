//! The one place `mult` draws unpredictable bytes.
//!
//! Three copies of this used to exist (state-file temp names, runtime-file temp
//! names, the instance token). They are all guarding the same thing — a path or
//! an id another local process must not be able to predict — so they now share
//! one implementation (F20).

use std::{fs::File, io, io::Read};

/// Eight bytes from `/dev/urandom`.
///
/// `read_exact` of a fixed length, never `read_to_end`: `/dev/urandom` never
/// reaches EOF. A caller that can proceed without randomness treats the error
/// as "no randomness available" rather than propagating it.
pub fn random_u64() -> io::Result<u64> {
    let mut bytes = [0_u8; 8];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(u64::from_ne_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_draws_differ() {
        // Not a randomness test: it only pins that the source is read afresh
        // each call rather than returning a constant.
        let first = random_u64().expect("read urandom");
        let second = random_u64().expect("read urandom");
        assert_ne!(first, second);
    }
}
