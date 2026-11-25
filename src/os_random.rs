//! Secret randomness straight from the operating system's CSPRNG
//! (`getrandom`, through `rand::rngs::SysRng`): tokens, key material, and
//! AES-GCM nonces. Never a userspace or seeded generator.

use rand::{TryRng as _, rngs::SysRng};

/// Fills `dst` from the OS CSPRNG. Panics if the OS cannot supply
/// randomness, as `rand` 0.8's `OsRng::fill_bytes` did: there is no safe
/// fallback for a secret.
pub fn fill(dst: &mut [u8]) {
    SysRng
        .try_fill_bytes(dst)
        .expect("the operating system's random number generator failed");
}

/// `N` bytes from the OS CSPRNG.
pub fn bytes<const N: usize>() -> [u8; N] {
    let mut bytes = [0_u8; N];
    fill(&mut bytes);
    bytes
}

#[cfg(test)]
mod tests {
    #[test]
    fn draws_differ() {
        assert_ne!(super::bytes::<32>(), super::bytes::<32>());
        let mut buffer = [0_u8; 64];
        super::fill(&mut buffer);
        assert_ne!(buffer, [0; 64]);
    }
}
