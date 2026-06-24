//! Share-target (difficulty gate) helper.
//!
//! The Midstate pool does NOT send the share target over the wire; the miner
//! computes it locally from a leading-zero-bit count and gates candidates so it
//! only submits nonces the pool will accept (pool default = 20 bits). A nonce
//! that also clears the network target is auto-detected as a block by the pool.

/// Build the 32-byte share gate from a leading-zero-bit count.
///
/// `bits = 20` → `00 00 0f ff ff … ff` (2 zero bytes, then `0xff >> 4 = 0x0f`).
/// Compared big-endian-lexicographically against the final hash (`final < target`).
pub fn share_target(bits: u32) -> [u8; 32] {
    let mut t = [0xffu8; 32];
    let full = ((bits / 8) as usize).min(32); // whole leading 0x00 bytes
    let rem = bits % 8; // remaining zero bits in the next byte
    for b in t.iter_mut().take(full) {
        *b = 0x00;
    }
    if full < 32 && rem > 0 {
        t[full] = 0xffu8 >> rem;
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bits_20_matches_pool_default() {
        let t = share_target(20);
        assert_eq!(t[0], 0x00);
        assert_eq!(t[1], 0x00);
        assert_eq!(t[2], 0x0f); // 0xff >> 4
        assert_eq!(t[3], 0xff);
        assert_eq!(t[31], 0xff);
    }

    #[test]
    fn whole_byte_boundaries() {
        let t8 = share_target(8);
        assert_eq!(t8[0], 0x00);
        assert_eq!(t8[1], 0xff);
        let t16 = share_target(16);
        assert_eq!(&t16[0..2], &[0x00, 0x00]);
        assert_eq!(t16[2], 0xff);
    }

    #[test]
    fn extremes() {
        assert_eq!(share_target(0), [0xff; 32]); // accept everything
        assert_eq!(share_target(256), [0x00; 32]); // accept nothing (saturates)
    }
}
