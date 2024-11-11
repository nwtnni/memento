use crossbeam_utils::CachePadded;
use etrace::some_or;

use crate::pmem::{Collectable, RootIdx};

use super::{
    super::{global_pool, PoolHandle},
    PAllocator,
};
use std::{
    mem::MaybeUninit,
    os::raw::{c_char, c_int, c_ulong, c_void},
    sync::atomic::AtomicUsize,
};

#[derive(Debug)]
pub struct Cxlalloc {}

impl PAllocator for Cxlalloc {
    unsafe fn open(filepath: *const libc::c_char, filesize: u64) -> libc::c_int {
        cxlalloc_static::cxlalloc_init(filepath, filesize as usize, 0, 64, 0, 1);
        (!cxlalloc_static::cxlalloc_is_clean()) as libc::c_int
    }

    unsafe fn create(filepath: *const libc::c_char, filesize: u64) -> libc::c_int {
        Self::open(filepath, filesize)
    }

    unsafe fn mmapped_addr() -> usize {
        cxlalloc_static::cxlalloc_offset_to_pointer(0) as usize
    }

    unsafe fn close(_start: usize, _len: usize) {}

    unsafe fn measure() -> usize {
        0
    }

    unsafe fn recover() -> libc::c_int {
        0
    }

    unsafe fn set_root(ptr: *mut libc::c_void, i: u64) -> *mut libc::c_void {
        let root = Self::get_root(i);
        cxlalloc_static::cxlalloc_set_root(i as usize, ptr);
        root
    }

    unsafe fn get_root(i: u64) -> *mut libc::c_void {
        cxlalloc_static::cxlalloc_get_root(i as usize)
    }

    unsafe fn malloc(sz: libc::c_ulong) -> *mut libc::c_void {
        cxlalloc_static::cxlalloc_malloc(sz as usize)
    }

    unsafe fn free(ptr: *mut libc::c_void, _len: usize) {
        cxlalloc_static::cxlalloc_free(ptr)
    }

    unsafe fn set_root_filter<T: Collectable>(_: u64) {}

    unsafe fn mark<T: Collectable>(_: &mut T, _: usize, _: &mut super::GarbageCollection) {}

    unsafe extern "C" fn filter_inner<T: Collectable>(
        _: *mut T,
        _: usize,
        _: &mut super::GarbageCollection,
    ) {
    }

    unsafe fn init_thread(tid: usize) {
        cxlalloc_static::cxlalloc_init_thread(tid)
    }

    unsafe fn gc() {}

    unsafe fn invalidate() {}
}
