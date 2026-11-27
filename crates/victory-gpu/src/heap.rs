//! Our own big blocks of memory are kept when they are released, for the
//! next that asks for one of the size (`VICTORY_HEAP_KEEP=1`; off by default,
//! every block from the system's heap: the unit tests run on it, the title
//! has not yet, and what it gains is not measured).
//!
//! A submission is prepared with tables and vectors of its steps, its
//! refreshes, the pages asked about: hundreds of KiB each for the frame's
//! large submissions, made, grown and released 37 times a frame. The system's
//! heap gives blocks of that size back to the system when they are released
//! and takes new memory for the next one, whose pages are then made one by
//! one as they are first written. On an RTX 5070 Ti, where the thread that
//! prepares submissions is what a frame waits for, 13 % of its samples were
//! under an allocation or a growing vector (session ab5), most of that in
//! the heap itself.
//!
//! Blocks of 64 KiB to 4 MiB are rounded up to one of four sizes to a
//! doubling and kept, a few of each size and [`KEPT_MOST`] bytes in all; a
//! vector that grows within its size stays where it is. Everything else goes
//! to the system's heap unchanged.
//!
//! Whether sizes are rounded is decided at the first allocation and never
//! changes: a block is released with the size it was made with.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU8, AtomicUsize, Ordering::AcqRel, Ordering::Relaxed};

/// The smallest and the largest block kept.
const LEAST: usize = 64 << 10;
const MOST: usize = 4 << 20;
/// Sizes to a doubling.
const STEPS: usize = 4;
/// The sizes: [`LEAST`] to [`MOST`], both among them.
const SIZES: usize = 6 * STEPS + 1;
/// Blocks kept of one size at most.
const SLOTS: usize = 6;
/// Bytes kept at most, about.
const KEPT_MOST: usize = 64 << 20;
/// What the system's heap aligns every block to; blocks that ask for more
/// are not kept.
const ALIGN: usize = 16;

/// The blocks kept, by size.
static KEPT: [[AtomicPtr<u8>; SLOTS]; SIZES] = [const { [const { AtomicPtr::new(std::ptr::null_mut()) }; SLOTS] }; SIZES];
/// Their bytes.
static KEPT_BYTES: AtomicUsize = AtomicUsize::new(0);
/// 0: not decided yet, 1: the system's heap alone, 2: sizes rounded.
static CHOICE: AtomicU8 = AtomicU8::new(0);
/// Whether released blocks are kept and kept ones given out now (diagnostics
/// builds turn it off and on while the title runs, `crate::ab`).
static KEEPING: AtomicBool = AtomicBool::new(true);

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetEnvironmentVariableW(name: *const u16, buffer: *mut u16, size: u32) -> u32;
}

/// Whether sizes are rounded. (The variable is read with the system's call
/// into memory on the stack: nothing here may allocate.)
#[inline]
fn rounding() -> bool {
    match CHOICE.load(Relaxed) {
        1 => false,
        2 => true,
        _ => decide(),
    }
}

#[cold]
fn decide() -> bool {
    const NAME: [u16; 18] = {
        let text = b"VICTORY_HEAP_KEEP\0";
        let mut wide = [0u16; 18];
        let mut i = 0;
        while i < 18 {
            wide[i] = text[i] as u16;
            i += 1;
        }
        wide
    };
    let mut value = [0u16; 8];
    // SAFETY: a name that ends in zero and a buffer of the length given.
    let n = unsafe { GetEnvironmentVariableW(NAME.as_ptr(), value.as_mut_ptr(), value.len() as u32) };
    // (The unit tests all run on it.)
    let on = cfg!(test) || n == 1 && value[0] == u16::from(b'1');
    // (Two threads that decide at once decide the same.)
    CHOICE.store(if on { 2 } else { 1 }, Relaxed);
    on
}

/// Diagnostics: released blocks are kept from now on, or not.
#[cfg(feature = "diagnostics")]
pub(crate) fn keeping(on: bool) {
    KEEPING.store(on, Relaxed);
}

/// The size a block of `size` bytes is made with, and its number among the
/// sizes; none for a block that is not kept.
fn sized(size: usize, align: usize) -> Option<(usize, usize)> {
    if !(LEAST..=MOST).contains(&size) || align > ALIGN {
        return None;
    }
    let doubling = size.ilog2() as usize;
    let (base, step) = (1usize << doubling, (1usize << doubling) / STEPS);
    let steps = (size - base).div_ceil(step);
    Some(((doubling - LEAST.ilog2() as usize) * STEPS + steps, base + steps * step))
}

/// [`sized`] of a layout, where sizes are rounded.
#[inline]
fn kept(layout: Layout) -> Option<(usize, Layout)> {
    if layout.size() < LEAST || !rounding() {
        return None;
    }
    let (number, bytes) = sized(layout.size(), layout.align())?;
    // SAFETY: a size of 4 MiB at most and a power of two to align to.
    Some((number, unsafe { Layout::from_size_align_unchecked(bytes, ALIGN) }))
}

/// A kept block of size `number`, if there is one.
fn take(number: usize, bytes: usize) -> Option<*mut u8> {
    if !KEEPING.load(Relaxed) {
        return None;
    }
    for slot in &KEPT[number] {
        if slot.load(Relaxed).is_null() {
            continue;
        }
        let block = slot.swap(std::ptr::null_mut(), AcqRel);
        if !block.is_null() {
            KEPT_BYTES.fetch_sub(bytes, Relaxed);
            return Some(block);
        }
    }
    None
}

/// Keeps a released block of size `number`, if there is room: whether it
/// did.
fn give(number: usize, bytes: usize, block: *mut u8) -> bool {
    if !KEEPING.load(Relaxed) || KEPT_BYTES.load(Relaxed) + bytes > KEPT_MOST {
        return false;
    }
    for slot in &KEPT[number] {
        if slot.load(Relaxed).is_null() && slot.compare_exchange(std::ptr::null_mut(), block, AcqRel, Relaxed).is_ok() {
            KEPT_BYTES.fetch_add(bytes, Relaxed);
            return true;
        }
    }
    false
}

/// The allocator everything of ours goes through.
pub(crate) struct Base;

// SAFETY: a block is one of the system heap's, made with the layout it is
// released with there (rounded the same way both times, as `CHOICE` never
// changes once set); a kept block is in one slot or with one owner.
unsafe impl GlobalAlloc for Base {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        match kept(layout) {
            // SAFETY: the caller's contract, with a size no smaller.
            Some((number, whole)) => take(number, whole.size()).unwrap_or_else(|| unsafe { System.alloc(whole) }),
            // SAFETY: the caller's contract, passed on.
            None => unsafe { System.alloc(layout) },
        }
    }

    #[inline]
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        match kept(layout) {
            Some((number, whole)) => match take(number, whole.size()) {
                Some(block) => {
                    // SAFETY: a block of at least the layout's size, ours alone.
                    unsafe { block.write_bytes(0, layout.size()) };
                    block
                }
                // SAFETY: the caller's contract, with a size no smaller.
                None => unsafe { System.alloc_zeroed(whole) },
            },
            // SAFETY: the caller's contract, passed on.
            None => unsafe { System.alloc_zeroed(layout) },
        }
    }

    #[inline]
    unsafe fn dealloc(&self, at: *mut u8, layout: Layout) {
        match kept(layout) {
            Some((number, whole)) => {
                if !give(number, whole.size(), at) {
                    // SAFETY: made with this layout (`alloc`).
                    unsafe { System.dealloc(at, whole) };
                }
            }
            // SAFETY: the caller's contract, passed on.
            None => unsafe { System.dealloc(at, layout) },
        }
    }

    #[inline]
    unsafe fn realloc(&self, at: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        // SAFETY: the caller's contract: `size` is a size its alignment allows.
        let new = unsafe { Layout::from_size_align_unchecked(size, layout.align()) };
        match (kept(layout), kept(new)) {
            // SAFETY: the caller's contract, passed on; the system's block.
            (None, None) => unsafe { System.realloc(at, layout, size) },
            // The same size: it has the room.
            (Some((was, _)), Some((will, _))) if was == will => at,
            _ => {
                // SAFETY: the caller's contract; the two blocks do not overlap.
                unsafe {
                    let moved = self.alloc(new);
                    if !moved.is_null() {
                        std::ptr::copy_nonoverlapping(at, moved, layout.size().min(size));
                        self.dealloc(at, layout);
                    }
                    moved
                }
            }
        }
    }
}

/// (Diagnostics builds count every block: `crate::copystat`'s allocator is
/// the global one there, over this one.)
#[cfg(not(feature = "diagnostics"))]
#[global_allocator]
static HEAP: Base = Base;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_are_four_to_a_doubling_and_hold_what_is_asked() {
        assert_eq!(sized(LEAST - 1, 8), None);
        assert_eq!(sized(MOST + 1, 8), None);
        assert_eq!(sized(LEAST, 32), None);
        assert_eq!(sized(LEAST, 8), Some((0, LEAST)));
        assert_eq!(sized(LEAST + 1, 8), Some((1, LEAST + LEAST / 4)));
        assert_eq!(sized(2 * LEAST, 16), Some((STEPS, 2 * LEAST)));
        assert_eq!(sized(MOST, 1), Some((SIZES - 1, MOST)));
        let mut last = (0, 0);
        for size in (LEAST..=MOST).step_by(4093) {
            let (number, bytes) = sized(size, 8).unwrap();
            assert!(bytes >= size && bytes - size <= size / 4 && number < SIZES, "{size}");
            assert!(number >= last.0 && bytes >= last.1, "{size}");
            assert_eq!(sized(bytes, 8), Some((number, bytes)), "a size is its own");
            last = (number, bytes);
        }
    }

    #[test]
    fn blocks_keep_their_bytes_as_they_grow_and_shrink() {
        let fill = |at: *mut u8, from: usize, to: usize| (from..to).for_each(|i| unsafe { at.add(i).write(i as u8 ^ (i >> 8) as u8) });
        let holds = |at: *mut u8, to: usize| (0..to).all(|i| unsafe { at.add(i).read() } == i as u8 ^ (i >> 8) as u8);
        let layout = |size: usize| Layout::from_size_align(size, 8).unwrap();
        // SAFETY: blocks made, grown and released with the layouts they have.
        unsafe {
            let mut size = 1000;
            let mut at = Base.alloc(layout(size));
            fill(at, 0, size);
            for next in [40_000, 70_000, 75_000, 200_000, 3 << 20, 6 << 20, 5 << 20, 100_000, 90_000, 500] {
                at = Base.realloc(at, layout(size), next);
                assert!(!at.is_null());
                assert!(holds(at, size.min(next)), "{size} to {next}");
                fill(at, size.min(next), next);
                size = next;
            }
            Base.dealloc(at, layout(size));
            // A kept block given out for zeroed memory is zeroed.
            let dirty = Base.alloc(layout(300_000));
            fill(dirty, 0, 300_000);
            Base.dealloc(dirty, layout(300_000));
            for _ in 0..SLOTS + 1 {
                let zeroed = Base.alloc_zeroed(layout(300_000));
                assert!((0..300_000).all(|i| zeroed.add(i).read() == 0));
                Base.dealloc(zeroed, layout(300_000));
            }
        }
    }
}
