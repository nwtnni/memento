use crossbeam_utils::CachePadded;
use etrace::some_or;

use crate::pmem::{Collectable, RootIdx};

use super::{
    super::{global_pool, PoolHandle},
    PAllocator,
};
use core::{
    ffi::CStr,
    ptr::{self, NonNull},
};
use std::{
    env,
    mem::MaybeUninit,
    os::raw::{c_char, c_int, c_ulong, c_void},
    sync::atomic::AtomicUsize,
};

#[derive(Debug)]
pub struct Cxlalloc {}

impl PAllocator for Cxlalloc {
    unsafe fn open(filepath: *const libc::c_char, filesize: u64) -> libc::c_int {
        cxlalloc_global::initialize_process(
            cxlalloc_global::Raw::builder()
                .size_small(filesize as usize / 2)
                .size_large(filesize as usize / 2)
                .backend(
                    cxlalloc_global::backend::Backend::builder()
                        .kind(cxlalloc_global::backend::Shm)
                        .maybe_numa(
                            env::var("CXL_NUMA_NODE")
                                .ok()
                                .and_then(|numa| numa.parse::<usize>().ok())
                                .map(|node| cxlalloc_global::backend::Numa::Bind { node }),
                        )
                        .build(),
                ),
            CStr::from_ptr(filepath)
                .to_str()
                .expect("Expected UTF-8 filepath")
                .trim_start_matches("/dev/shm"),
        );
        (!cxlalloc_global::is_clean()) as libc::c_int
    }

    unsafe fn create(filepath: *const libc::c_char, filesize: u64) -> libc::c_int {
        Self::open(filepath, filesize)
    }

    #[inline]
    unsafe fn mmapped_addr() -> usize {
        cxlalloc_global::offset_to_pointer(0).as_ptr() as usize
    }

    #[inline]
    unsafe fn close(_start: usize, _len: usize) {}

    #[inline]
    unsafe fn cache_count() -> usize {
        0
    }

    #[inline]
    unsafe fn cache_size() -> usize {
        0
    }

    #[inline]
    unsafe fn recover() -> libc::c_int {
        0
    }

    #[inline]
    unsafe fn set_root(ptr: *mut libc::c_void, i: u64) -> *mut libc::c_void {
        let root = Self::get_root(i);
        cxlalloc_global::set_root_untyped(i as usize, ptr);
        root
    }

    #[inline]
    unsafe fn get_root(i: u64) -> *mut libc::c_void {
        cxlalloc_global::root_untyped(i as usize)
            .map(NonNull::as_ptr)
            .unwrap_or_else(ptr::null_mut)
    }

    #[inline]
    unsafe fn malloc(sz: libc::c_ulong) -> *mut libc::c_void {
        cxlalloc_global::allocate_untyped(sz as usize)
            .map(NonNull::as_ptr)
            .unwrap_or_else(ptr::null_mut)
    }

    #[inline]
    unsafe fn free(ptr: *mut libc::c_void, _len: usize) {
        cxlalloc_global::deallocate_untyped(ptr)
    }

    #[inline]
    unsafe fn set_root_filter<T: Collectable>(_: u64) {}

    #[inline]
    unsafe fn mark<T: Collectable>(_: &mut T, _: usize, _: &mut super::GarbageCollection) {}

    #[inline]
    unsafe extern "C" fn filter_inner<T: Collectable>(
        _: *mut T,
        _: usize,
        _: &mut super::GarbageCollection,
    ) {
    }

    #[inline]
    unsafe fn init_thread(tid: usize) {
        cxlalloc_global::initialize_thread(u16::try_from(tid).unwrap() - 1)
    }

    #[inline]
    unsafe fn gc() {}

    #[inline]
    unsafe fn gc_count() -> usize {
        0
    }

    #[inline]
    unsafe fn gc_time() -> usize {
        0
    }

    #[inline]
    unsafe fn invalidate() {}
}
