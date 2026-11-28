//! Process-wide allocation counters for benchmark-only attribution.
//!
//! These counters count allocator requests, not live memory and not memcpy
//! calls. They are used to compare identical direct and routed workloads in
//! the same process; production binaries do not use this allocator.

use std::{
    alloc::{GlobalAlloc, Layout, System},
    sync::atomic::{AtomicU64, Ordering},
};

pub struct CountingAllocator;

static ALLOCATION_COUNT: AtomicU64 = AtomicU64::new(0);
static ALLOCATED_BYTES: AtomicU64 = AtomicU64::new(0);

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATION_COUNT.fetch_add(1, Ordering::Relaxed);
        ALLOCATED_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        // SAFETY: `layout` is passed through unchanged to the system allocator.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller upholds the global allocator contract; the layout
        // is passed through unchanged to the system allocator.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATION_COUNT.fetch_add(1, Ordering::Relaxed);
        ALLOCATED_BYTES.fetch_add(new_size as u64, Ordering::Relaxed);
        // SAFETY: the caller upholds the global allocator contract; arguments
        // are passed through unchanged to the system allocator.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

#[derive(Clone, Copy, Debug)]
pub struct AllocationSnapshot {
    pub count: u64,
    pub bytes: u64,
}

pub fn snapshot() -> AllocationSnapshot {
    AllocationSnapshot {
        count: ALLOCATION_COUNT.load(Ordering::Relaxed),
        bytes: ALLOCATED_BYTES.load(Ordering::Relaxed),
    }
}

pub fn delta_since(start: AllocationSnapshot) -> AllocationSnapshot {
    let end = snapshot();
    AllocationSnapshot {
        count: end.count.saturating_sub(start.count),
        bytes: end.bytes.saturating_sub(start.bytes),
    }
}
