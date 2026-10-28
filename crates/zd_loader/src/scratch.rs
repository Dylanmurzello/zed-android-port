//! Working memory for the trap handler, claimed per invocation.
//!
//! The handler runs on whatever stack the trap finds. With `SA_ONSTACK`
//! that is the alternate stack a thread registered, and the smallest in
//! common use (Rust's 16 KiB, musl's 12 KiB) hold little beyond the
//! kernel's own frame of about 5 KiB. Path buffers and the rest of the
//! handler's working memory therefore live here, in a pool indexed by
//! a bitmap: concurrent threads, and a nested invocation on one thread
//! (a guest signal handler making a path syscall on top of this one),
//! each take their own entry, released when the invocation returns.
//! A `fork` child inherits the bits set by the parent's other threads
//! and never clears them, which costs a child that keeps running the
//! same image a few entries; an exec starts with a clean pool.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU64, Ordering};

use zd_engine_core::path::MAX_PATH;

use crate::sys;

pub struct Scratch {
    pub paths: [[u8; MAX_PATH]; 4],
    pub bytes: [u8; 1024],
}

impl Scratch {
    pub const fn new() -> Self {
        Self {
            paths: [[0; MAX_PATH]; 4],
            bytes: [0; 1024],
        }
    }
}

const ENTRIES: usize = 64;
/// Attempts before an exhausted pool is reported as such.
const YIELDS: usize = 64;

struct Pool(UnsafeCell<[Scratch; ENTRIES]>);

// SAFETY: an entry is reachable only through the `Claim` holding its
// bit, so no two references to one entry exist at a time.
unsafe impl Sync for Pool {}

static POOL: Pool = Pool(UnsafeCell::new([const { Scratch::new() }; ENTRIES]));
static BUSY: AtomicU64 = AtomicU64::new(0);

pub struct Claim {
    index: usize,
}

impl Claim {
    pub fn get(&mut self) -> &mut Scratch {
        // SAFETY: this claim holds the entry's bit; see `Pool`.
        unsafe { &mut (*POOL.0.get())[self.index] }
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        BUSY.fetch_and(!(1u64 << self.index), Ordering::Release);
    }
}

pub fn claim() -> Option<Claim> {
    let mut yields = 0;
    loop {
        let busy = BUSY.load(Ordering::Relaxed);
        let free = !busy;
        if free == 0 {
            if yields == YIELDS {
                return None;
            }
            yields += 1;
            sys::sched_yield();
            continue;
        }
        let index = free.trailing_zeros() as usize;
        if BUSY
            .compare_exchange_weak(busy, busy | (1u64 << index), Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            return Some(Claim { index });
        }
    }
}

/// One invocation's working memory: a pool entry, or a stack copy when
/// the pool is exhausted.
pub enum Working<'a> {
    Pooled(Claim),
    Stack(&'a mut Scratch),
}

impl Working<'_> {
    pub fn get(&mut self) -> &mut Scratch {
        match self {
            Working::Pooled(claim) => claim.get(),
            Working::Stack(scratch) => scratch,
        }
    }
}
