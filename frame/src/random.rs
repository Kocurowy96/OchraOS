// SPDX-License-Identifier: GPL-2.0-only

//! The kernel's random number generator: ChaCha20 (RFC 8439) as a CSPRNG.
//!
//! A CSPRNG ("cryptographically secure pseudo-random number generator") stretches a secret
//! seed into as many unpredictable bytes as needed. Ours runs the ChaCha20 block function with
//! a 256-bit key and a counter, and after every request replaces its key with fresh output
//! ("fast key erasure"), so someone who later reads the generator's state cannot reconstruct
//! what it handed out before.
//!
//! The seed comes from the CPU: RDSEED (the raw hardware entropy source) and RDRAND (a
//! hardware DRBG seeded from it), each used only if CPUID reports it and a sanity check
//! passes — some AMD CPUs had a bug where RDRAND returned all ones after suspend. The time
//! stamp counter is mixed in too. If neither instruction is usable, the generator still runs
//! on the TSC alone, and [`init`] reports that the seed is weak.
//!
//! Users: `AT_RANDOM` for new programs, address space layout randomisation, `getrandom`.

use core::arch::asm;
use core::arch::x86_64::{__cpuid, __cpuid_count, _rdtsc};
use core::fmt;

use spin::Once;

use crate::sync::IrqMutex;

/// "expand 32-byte k": the ChaCha constants.
const CONSTANTS: [u32; 4] = [0x6170_7865, 0x3320_646e, 0x7962_2d32, 0x6b20_6574];

/// One ChaCha20 block (RFC 8439, section 2.3): 64 bytes of key stream for `key`, `counter`
/// and `nonce`.
pub fn chacha20_block(key: &[u32; 8], counter: u32, nonce: &[u32; 3]) -> [u8; 64] {
    let mut initial = [0u32; 16];
    initial[..4].copy_from_slice(&CONSTANTS);
    initial[4..12].copy_from_slice(key);
    initial[12] = counter;
    initial[13..].copy_from_slice(nonce);

    let mut state = initial;
    for _ in 0..10 {
        // A "double round": four quarter rounds on the columns, four on the diagonals.
        quarter_round(&mut state, 0, 4, 8, 12);
        quarter_round(&mut state, 1, 5, 9, 13);
        quarter_round(&mut state, 2, 6, 10, 14);
        quarter_round(&mut state, 3, 7, 11, 15);
        quarter_round(&mut state, 0, 5, 10, 15);
        quarter_round(&mut state, 1, 6, 11, 12);
        quarter_round(&mut state, 2, 7, 8, 13);
        quarter_round(&mut state, 3, 4, 9, 14);
    }

    let mut output = [0u8; 64];
    for (index, (word, start)) in state.iter().zip(initial).enumerate() {
        let bytes = word.wrapping_add(start).to_le_bytes();
        output[index * 4..index * 4 + 4].copy_from_slice(&bytes);
    }
    output
}

fn quarter_round(state: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
    state[a] = state[a].wrapping_add(state[b]);
    state[d] = (state[d] ^ state[a]).rotate_left(16);
    state[c] = state[c].wrapping_add(state[d]);
    state[b] = (state[b] ^ state[c]).rotate_left(12);
    state[a] = state[a].wrapping_add(state[b]);
    state[d] = (state[d] ^ state[a]).rotate_left(8);
    state[c] = state[c].wrapping_add(state[d]);
    state[b] = (state[b] ^ state[c]).rotate_left(7);
}

/// The generator state: a key, and a counter for the blocks made with it.
struct Generator {
    key: [u32; 8],
    counter: u32,
}

impl Generator {
    fn next_block(&mut self) -> [u8; 64] {
        let block = chacha20_block(&self.key, self.counter, &[0; 3]);
        self.counter += 1;
        block
    }

    fn fill(&mut self, buffer: &mut [u8]) {
        for chunk in buffer.chunks_mut(64) {
            let block = self.next_block();
            chunk.copy_from_slice(&block[..chunk.len()]);
        }
        // Fast key erasure: a fresh key from a block nobody saw, counter back to 0.
        let block = self.next_block();
        self.key = words(&block[..32]);
        self.counter = 0;
    }
}

static GENERATOR: Once<IrqMutex<Generator>> = Once::new();

/// Fills `buffer` with random bytes.
pub fn fill(buffer: &mut [u8]) {
    GENERATOR
        .get()
        .expect("random::init runs at boot")
        .lock()
        .fill(buffer);
}

/// A random 64-bit number.
pub fn u64() -> u64 {
    let mut bytes = [0; 8];
    fill(&mut bytes);
    u64::from_le_bytes(bytes)
}

/// How a hardware random source fared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Used,
    /// CPUID does not report the instruction.
    Missing,
    /// It failed or returned suspicious values (all zeros, all ones, repeats).
    Broken,
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(match self {
            Self::Used => "used",
            Self::Missing => "missing",
            Self::Broken => "broken, ignored",
        })
    }
}

/// Where the seed came from, for the boot log.
#[derive(Clone, Copy, Debug)]
pub struct Seeding {
    pub rdseed: Source,
    pub rdrand: Source,
}

impl Seeding {
    /// Neither hardware source was usable: the seed is only the time stamp counter.
    pub fn is_weak(&self) -> bool {
        self.rdseed != Source::Used && self.rdrand != Source::Used
    }
}

/// Seeds the generator. Call once, on the boot CPU, early.
pub fn init() -> Seeding {
    let has_rdrand = __cpuid(1).ecx & (1 << 30) != 0;
    let has_rdseed = __cpuid_count(7, 0).ebx & (1 << 18) != 0;

    let rdseed = if has_rdseed { sample(rdseed) } else { None };
    let rdrand = if has_rdrand { sample(rdrand) } else { None };
    let status = |present: bool, values: &Option<[u64; 4]>| match (present, values) {
        (false, _) => Source::Missing,
        (true, None) => Source::Broken,
        (true, Some(_)) => Source::Used,
    };
    let seeding = Seeding {
        rdseed: status(has_rdseed, &rdseed),
        rdrand: status(has_rdrand, &rdrand),
    };

    // Everything is XORed into one key; then one ChaCha block, with time stamps as the nonce,
    // spreads every input bit over the whole key.
    let mut material = [0u64; 4];
    for values in [rdseed, rdrand].into_iter().flatten() {
        for (slot, value) in material.iter_mut().zip(values) {
            *slot ^= value;
        }
    }
    let mut key = [0u32; 8];
    for (index, value) in material.iter().enumerate() {
        key[index * 2] = *value as u32;
        key[index * 2 + 1] = (*value >> 32) as u32;
    }
    let time = [timestamp(), timestamp(), timestamp()];
    let nonce = [
        time[0] as u32 ^ (time[2] >> 32) as u32,
        time[1] as u32,
        time[2] as u32,
    ];
    let mixed = chacha20_block(&key, (time[0] >> 32) as u32, &nonce);

    GENERATOR.call_once(|| {
        IrqMutex::new(Generator {
            key: words(&mixed[..32]),
            counter: 0,
        })
    });
    seeding
}

/// Four values from `source`, or `None` if it fails or they look wrong: zero, all ones, or
/// two equal values (64-bit random values repeat practically never).
fn sample(source: fn() -> Option<u64>) -> Option<[u64; 4]> {
    let mut values = [0; 4];
    for index in 0..values.len() {
        let value = source()?;
        if value == 0 || value == u64::MAX || values[..index].contains(&value) {
            return None;
        }
        values[index] = value;
    }
    Some(values)
}

/// RDRAND, retried a few times as Intel recommends (it can run dry briefly).
fn rdrand() -> Option<u64> {
    (0..10).find_map(|_| {
        let value: u64;
        let ok: u8;
        // SAFETY: only called after CPUID reported RDRAND; it writes two registers.
        unsafe {
            asm!("rdrand {}", "setc {}", out(reg) value, out(reg_byte) ok, options(nomem, nostack))
        };
        (ok != 0).then_some(value)
    })
}

/// RDSEED, retried more often: the raw entropy source is slower to refill.
fn rdseed() -> Option<u64> {
    (0..100).find_map(|_| {
        let value: u64;
        let ok: u8;
        // SAFETY: only called after CPUID reported RDSEED; it writes two registers.
        unsafe {
            asm!("rdseed {}", "setc {}", out(reg) value, out(reg_byte) ok, options(nomem, nostack))
        };
        if ok == 0 {
            core::hint::spin_loop();
        }
        (ok != 0).then_some(value)
    })
}

fn timestamp() -> u64 {
    // SAFETY: RDTSC exists on every x86-64 CPU and only reads the counter.
    unsafe { _rdtsc() }
}

fn words(bytes: &[u8]) -> [u32; 8] {
    let mut words = [0; 8];
    let (chunks, _) = bytes.as_chunks::<4>();
    for (word, chunk) in words.iter_mut().zip(chunks) {
        *word = u32::from_le_bytes(*chunk);
    }
    words
}
