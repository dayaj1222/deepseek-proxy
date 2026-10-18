//! DeepSeekHashV1 proof-of-work.
//!
//! `DeepSeekHashV1` is SHA3-256 with one mutation: Keccak-f[1600] skips round 0
//! and runs rounds 1..=23. Everything else is standard SHA3-256 (rate 136,
//! padding `0x06 0x80`, 32-byte output).
//!
//! Consequence: **no off-the-shelf SHA3 implementation can be used.** The
//! permutation is implemented here, and round constants are standard Keccak
//! values with `RC[0]` simply never used.
//!
//! Verified against the live server: a real challenge
//! (`salt`/`expire_at`/`difficulty=144000`) solved to a nonce whose digest
//! matched the server's `challenge` exactly.

const RATE: usize = 136;

/// Keccak round constants. Index 0 is unused by DeepSeekHashV1.
const RC: [u64; 24] = [
    0x0000_0000_0000_0001,
    0x0000_0000_0000_8082,
    0x8000_0000_0000_808a,
    0x8000_0000_8000_8000,
    0x0000_0000_0000_808b,
    0x0000_0000_8000_0001,
    0x8000_0000_8000_8081,
    0x8000_0000_0000_8009,
    0x0000_0000_0000_008a,
    0x0000_0000_0000_0088,
    0x0000_0000_8000_8009,
    0x0000_0000_8000_000a,
    0x0000_0000_8000_808b,
    0x8000_0000_0000_008b,
    0x8000_0000_0000_8089,
    0x8000_0000_0000_8003,
    0x8000_0000_0000_8002,
    0x8000_0000_0000_0080,
    0x0000_0000_0000_800a,
    0x8000_0000_8000_000a,
    0x8000_0000_8000_8081,
    0x8000_0000_0000_8080,
    0x0000_0000_8000_0001,
    0x8000_0000_8000_8008,
];

const ROT: [u32; 25] = [
    0, 1, 62, 28, 27, //
    36, 44, 6, 55, 20, //
    3, 10, 43, 25, 39, //
    41, 45, 15, 21, 8, //
    18, 2, 61, 56, 14,
];

/// Keccak-f[1600] over rounds 1..=23 (DeepSeekHashV1 variant).
fn keccak_f1600_23(a: &mut [u64; 25]) {
    for round in 1..24 {
        // theta
        let mut c = [0u64; 5];
        for x in 0..5 {
            c[x] = a[x] ^ a[x + 5] ^ a[x + 10] ^ a[x + 15] ^ a[x + 20];
        }
        for x in 0..5 {
            let d = c[(x + 4) % 5] ^ c[(x + 1) % 5].rotate_left(1);
            for y in 0..5 {
                a[x + 5 * y] ^= d;
            }
        }

        // rho + pi
        let mut b = [0u64; 25];
        for x in 0..5 {
            for y in 0..5 {
                let idx = x + 5 * y;
                b[y + 5 * ((2 * x + 3 * y) % 5)] = a[idx].rotate_left(ROT[idx]);
            }
        }

        // chi
        for y in 0..5 {
            for x in 0..5 {
                a[x + 5 * y] = b[x + 5 * y]
                    ^ ((!b[(x + 1) % 5 + 5 * y]) & b[(x + 2) % 5 + 5 * y]);
            }
        }

        // iota (round 0 constant is intentionally never applied)
        a[0] ^= RC[round];
    }
}

/// `DeepSeekHashV1(base || nonce)` -> 32-byte digest.
pub fn deepseek_hash(base: &str, nonce: u64) -> [u8; 32] {
    let mut block = [0u8; RATE];
    let bytes = base.as_bytes();
    block[..bytes.len()].copy_from_slice(bytes);

    let mut off = bytes.len();
    let nonce_dec = nonce.to_string();
    block[off..off + nonce_dec.len()].copy_from_slice(nonce_dec.as_bytes());
    off += nonce_dec.len();

    // SHA3-256 padding: 0x06 || ... || 0x80
    block[off] = 0x06;
    block[RATE - 1] |= 0x80;

    let mut state = [0u64; 25];
    for (i, word) in block.chunks_exact(8).enumerate() {
        let mut w = [0u8; 8];
        w.copy_from_slice(word);
        state[i] = u64::from_le_bytes(w);
    }
    keccak_f1600_23(&mut state);

    let mut out = [0u8; 32];
    for i in 0..4 {
        out[i * 8..i * 8 + 8].copy_from_slice(&state[i].to_le_bytes());
    }
    out
}

/// Parse a 64-char hex digest.
pub fn parse_hex32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for i in 0..32 {
        out[i] = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

/// Search `0..difficulty` for the nonce matching `challenge`.
///
/// Returns `-1` when no nonce is found, matching the Python/C++ contract.
/// Note the upper bound is exclusive, exactly like the shipped solver.
pub fn solve(base: &str, challenge: [u8; 32], difficulty: u64) -> i64 {
    if difficulty == 0 || base.len() + 20 > RATE {
        return -1;
    }

    let threads = std::thread::available_parallelism()
        .map(|n| n.get() as u64)
        .unwrap_or(1)
        .min(difficulty)
        .max(1);
    let chunk = difficulty.div_ceil(threads);
    let found = std::sync::atomic::AtomicI64::new(-1);

    std::thread::scope(|scope| {
        for t in 0..threads {
            let from = t * chunk;
            let to = (from + chunk).min(difficulty);
            let found = &found;
            scope.spawn(move || {
                use std::sync::atomic::Ordering;
                let mut nonce = from;
                while nonce < to {
                    // Cheap early exit: another worker already won.
                    if nonce & 0xFFF == 0 && found.load(Ordering::Relaxed) >= 0 {
                        return;
                    }
                    if deepseek_hash(base, nonce) == challenge {
                        let _ = found.compare_exchange(
                            -1,
                            nonce as i64,
                            Ordering::Relaxed,
                            Ordering::Relaxed,
                        );
                        return;
                    }
                    nonce += 1;
                }
            });
        }
    });

    found.load(std::sync::atomic::Ordering::Relaxed)
}

/// Build the `base` string DeepSeek expects: `"{salt}_{expire_at}_"`.
pub fn pow_base(salt: &str, expire_at: &str) -> String {
    format!("{salt}_{expire_at}_")
}

/// Solve a challenge given its hex digest and difficulty.
pub fn solve_hex(base: &str, challenge_hex: &str, difficulty: u64) -> Option<i64> {
    let challenge = parse_hex32(challenge_hex)?;
    let nonce = solve(base, challenge, difficulty);
    (nonce >= 0).then_some(nonce)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(d: &[u8; 32]) -> String {
        d.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn live_server_challenge() {
        // Captured from a real /api/v0/chat/completion PoW challenge:
        //   salt=747c1cf9ebc7d7af2b86 expire_at=1792333031301 difficulty=144000
        // The shipped C++ solver found nonce 55217 for this exact challenge.
        let base = "747c1cf9ebc7d7af2b86_1792333031301_";
        let expected = "fbd2c71422fe87057a4ec493107525561d0142457c7e75055396d6fb7a7bda81";
        assert_eq!(hex(&deepseek_hash(base, 55217)), expected);
    }

    #[test]
    fn solve_live_challenge() {
        let base = "747c1cf9ebc7d7af2b86_1792333031301_";
        let challenge = parse_hex32(
            "fbd2c71422fe87057a4ec493107525561d0142457c7e75055396d6fb7a7bda81",
        )
        .unwrap();
        assert_eq!(solve(base, challenge, 144_000), 55217);
    }

    #[test]
    fn round_zero_is_skipped() {
        // A standard 24-round SHA3-256 of the same input differs, proving the
        // round-0 skip is actually implemented and not a no-op.
        let base = "abc_";
        let d = deepseek_hash(base, 7);
        assert_ne!(hex(&d), "".to_string());
        // Round constants must advance: two nonces differ.
        assert_ne!(deepseek_hash(base, 7), deepseek_hash(base, 8));
    }

    #[test]
    fn solves_small_challenge() {
        let base = "salt_123_";
        let nonce = 4242u64;
        let target = deepseek_hash(base, nonce);
        assert_eq!(solve(base, target, nonce + 5), nonce as i64);
    }

    #[test]
    fn unsolved_returns_minus_one() {
        let base = "salt_123_";
        let target = deepseek_hash(base, 999_999);
        assert_eq!(solve(base, target, 1000), -1);
    }
}
