//! Persistent Atomic Pointer (persistent version of crossbeam_epoch atomic.rs)
//!
//! The difference from crossbeam is that pointers have relative address, not absolute address.
//!
//! - prev
//!     - pointer that pointing absolute  addr: `raw: *const T`
//!     - abosulte addr: `raw: usize`
//!     - function: `from_raw(raw: *mut T) -> Owned<T>`
//! - current
//!     - pointer that pointing relative addr: `ptr: PPtr<T>`
//!     - relative addr: `offset: usize`
//!     - function: `from_ptr(ptr: PPtr<T>) -> Owned<T>`

use core::cmp;
use core::fmt;
use core::marker::PhantomData;
use core::mem::{self, MaybeUninit};
use core::slice;
use core::sync::atomic::Ordering;

use super::Guard;
use crate::impl_left_bits;
use crate::ploc::Handle;
use crate::pmem::{global_pool, pool::PoolHandle, ptr::PPtr, Collectable, GarbageCollection};
use crate::PDefault;
use crossbeam_epoch::unprotected;
use crossbeam_utils::atomic::AtomicConsume;
use std::alloc;
use std::sync::atomic::AtomicUsize;

/// Given ordering for the success case in a compare-exchange operation, returns the strongest
/// appropriate ordering for the failure case.
#[inline]
fn strongest_failure_ordering(ord: Ordering) -> Ordering {
    use self::Ordering::*;
    match ord {
        Relaxed | Release => Relaxed,
        Acquire | AcqRel => Acquire,
        _ => SeqCst,
    }
}

/// The error returned on failed compare-and-set operation.
// TODO(crossbeam): remove in the next major version.
#[deprecated(note = "Use `CompareExchangeError` instead")]
pub type CompareAndSetError<'g, T, P> = CompareExchangeError<'g, T, P>;

/// The error returned on failed compare-and-swap operation.
pub struct CompareExchangeError<'g, T: ?Sized + Pointable, P: Pointer<T>> {
    /// The value in the atomic pointer at the time of the failed operation.
    pub current: PShared<'g, T>,

    /// The new value, which the operation failed to store.
    pub new: P,
}

impl<T, P: Pointer<T> + fmt::Debug> fmt::Debug for CompareExchangeError<'_, T, P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CompareExchangeError")
            .field("current", &self.current)
            .field("new", &self.new)
            .finish()
    }
}

/// Memory orderings for compare-and-set operations.
///
/// A compare-and-set operation can have different memory orderings depending on whether it
/// succeeds or fails. This trait generalizes different ways of specifying memory orderings.
///
/// The two ways of specifying orderings for compare-and-set are:
///
/// 1. Just one `Ordering` for the success case. In case of failure, the strongest appropriate
///    ordering is chosen.
/// 2. A pair of `Ordering`s. The first one is for the success case, while the second one is
///    for the failure case.
// TODO(crossbeam): remove in the next major version.
#[deprecated(
    note = "`compare_and_set` and `compare_and_set_weak` that use this trait are deprecated, \
            use `compare_exchange` or `compare_exchange_weak instead`"
)]
pub trait CompareAndSetOrdering {
    /// The ordering of the operation when it succeeds.
    fn success(&self) -> Ordering;

    /// The ordering of the operation when it fails.
    ///
    /// The failure ordering can't be `Release` or `AcqRel` and must be equivalent or weaker than
    /// the success ordering.
    fn failure(&self) -> Ordering;
}

#[allow(deprecated)]
impl CompareAndSetOrdering for Ordering {
    #[inline]
    fn success(&self) -> Ordering {
        *self
    }

    #[inline]
    fn failure(&self) -> Ordering {
        strongest_failure_ordering(*self)
    }
}

#[allow(deprecated)]
impl CompareAndSetOrdering for (Ordering, Ordering) {
    #[inline]
    fn success(&self) -> Ordering {
        self.0
    }

    #[inline]
    fn failure(&self) -> Ordering {
        self.1
    }
}

// auxiliary bit: 0b100000000000000000000000000000000000000000000000000000000000000000 in 64-bit
// Used for:
// - Detectable CAS: Indicating CAS parity (Odd/Even)
// - Insert: Indicating if the pointer is persisted
pub(crate) const POS_AUX_BITS: u32 = 0;
pub(crate) const NR_AUX_BITS: u32 = 1;
impl_left_bits!(aux_bits, POS_AUX_BITS, NR_AUX_BITS, usize);

// descriptor bit: 0b0100000000000000000000000000000000000000000000000000000000000000 in 64-bit
pub(crate) const POS_DESC_BITS: u32 = POS_AUX_BITS + NR_AUX_BITS;
pub(crate) const NR_DESC_BITS: u32 = 1;
impl_left_bits!(desc_bits, POS_DESC_BITS, NR_DESC_BITS, usize);

// tid bits: 0b0011111111100000000000000000000000000000000000000000000000000000 in 64-bit
const POS_TID_BITS: u32 = POS_DESC_BITS + NR_DESC_BITS;
const NR_TID_BITS: u32 = 9;
impl_left_bits!(tid_bits, POS_TID_BITS, NR_TID_BITS, usize);

// high bits: 0b0000000000011111111111111000000000000000000000000000000000000000 in 64-bit
const POS_HIGH_BITS: u32 = POS_TID_BITS + NR_TID_BITS;
const NR_HIGH_BITS: u32 = 11;
impl_left_bits!(high_bits, POS_HIGH_BITS, NR_HIGH_BITS, usize);

/// Cut as the length of high tag
#[inline]
pub fn cut_as_high_tag_len(raw: usize) -> usize {
    raw & !(usize::MAX << NR_HIGH_BITS)
}

/// Returns a bitmask containing the unused least significant bits of an aligned pointer to `T`.
#[inline]
fn low_bits<T: ?Sized + Pointable>() -> usize {
    (1 << T::ALIGN.trailing_zeros()) - 1
}

/// Panics if the pointer is not properly unaligned.
#[inline]
fn ensure_aligned<T: ?Sized + Pointable>(offset: usize) {
    assert_eq!(offset & low_bits::<T>(), 0, "unaligned pointer");
}

/// Given a tagged pointer `data`, returns the same pointer, but tagged with `tag`.
///
/// `tag` is truncated to fit into the unused bits of the pointer to `T`.
#[inline]
fn compose_tag<T: ?Sized + Pointable>(data: usize, ltag: usize) -> usize {
    (data & !low_bits::<T>()) | (ltag & low_bits::<T>())
}

#[inline]
fn compose_tid(tid: usize, data: usize) -> usize {
    (tid_bits() & (tid.rotate_right(POS_TID_BITS + NR_TID_BITS))) | (!tid_bits() & data)
}

#[inline]
fn compose_high_tag(htag: usize, data: usize) -> usize {
    (high_bits() & (htag.rotate_right(POS_HIGH_BITS + NR_HIGH_BITS))) | (!high_bits() & data)
}

/// Compose aux bit (1-bit, MSB)
#[inline]
fn compose_desc_bit(desc_bit: usize, data: usize) -> usize {
    (desc_bits() & (desc_bit.rotate_right(POS_DESC_BITS + NR_DESC_BITS))) | (!desc_bits() & data)
}

/// Compose aux bit (1-bit, MSB)
#[inline]
fn compose_aux_bit(aux_bit: usize, data: usize) -> usize {
    (aux_bits() & (aux_bit.rotate_right(POS_AUX_BITS + NR_AUX_BITS))) | (!aux_bits() & data)
}

/// Decomposes a tagged pointer `data` into the pointer and the tag.
/// (aux, desc, tid, high_tag, ptr, low_tag)
#[inline]
fn decompose_tag<T: ?Sized + Pointable>(data: usize) -> (usize, usize, usize, usize, usize, usize) {
    (
        (data & aux_bits()).rotate_left(POS_AUX_BITS + NR_AUX_BITS),
        (data & desc_bits()).rotate_left(POS_DESC_BITS + NR_DESC_BITS),
        (data & tid_bits()).rotate_left(POS_TID_BITS + NR_TID_BITS),
        (data & high_bits()).rotate_left(POS_HIGH_BITS + NR_HIGH_BITS),
        data & !aux_bits() & !desc_bits() & !tid_bits() & !high_bits() & !low_bits::<T>(),
        data & low_bits::<T>(),
    )
}

/// Types that are pointed to by a single word.
///
/// In concurrent programming, it is necessary to represent an object within a word because atomic
/// operations (e.g., reads, writes, read-modify-writes) support only single words.  This trait
/// qualifies such types that are pointed to by a single word.
///
/// The trait generalizes `Box<T>` for a sized type `T`.  In a box, an object of type `T` is
/// allocated in heap and it is owned by a single-word pointer.  This trait is also implemented for
/// `[MaybeUninit<T>]` by storing its size along with its elements and pointing to the pair of array
/// size and elements.
///
/// Pointers to `Pointable` types can be stored in [`PAtomic`], [`POwned`], and [`PShared`].  In
/// particular, Crossbeam supports dynamically sized slices as follows.
///
/// ```
/// # use memento::pmem::pool::*;
/// # use memento::*;
/// # use memento::test_utils::tests::get_dummy_handle;
/// # let pool = get_dummy_handle(8 * 1024 * 1024 * 1024).unwrap();
/// use std::mem::MaybeUninit;
/// use memento::pepoch::POwned;
///
/// // Assume there are PoolHandle, `pool`
/// let o = POwned::<[MaybeUninit<i32>]>::init(10, &pool); // allocating [i32; 10]
/// ```
pub trait Pointable {
    /// The alignment of pointer.
    const ALIGN: usize;

    /// The type for initializers.
    type Init;

    /// Initializes a with the given initializer in the pool.
    ///
    /// # Safety
    ///
    /// The result should be a multiple of `ALIGN`.
    unsafe fn init(init: Self::Init, pool: &PoolHandle) -> usize;

    /// Dereferences the given offset in the pool.
    ///
    /// # Safety
    ///
    /// - The given `offset` should have been initialized with [`Pointable::init`].
    /// - `offset` should not have yet been dropped by [`Pointable::drop`].
    /// - `offset` should not be mutably dereferenced by [`Pointable::deref_mut`] concurrently.
    unsafe fn deref(offset: usize, pool: &PoolHandle) -> &Self;

    /// Mutably dereferences the given offset in the pool.
    ///
    /// # Safety
    ///
    /// - The given `offset` should have been initialized with [`Pointable::init`].
    /// - `offset` should not have yet been dropped by [`Pointable::drop`].
    /// - `offset` should not be dereferenced by [`Pointable::deref`] or [`Pointable::deref_mut`]
    ///   concurrently.
    #[allow(clippy::mut_from_ref)]
    unsafe fn deref_mut(offset: usize, pool: &PoolHandle) -> &mut Self;

    /// Drops the object pointed to by the given offset in the pool.
    ///
    /// # Safety
    ///
    /// - The given `offset` should have been initialized with [`Pointable::init`].
    /// - `offset` should not have yet been dropped by [`Pointable::drop`].
    /// - `offset` should not be dereferenced by [`Pointable::deref`] or [`Pointable::deref_mut`]
    ///   concurrently.
    unsafe fn drop(offset: usize, pool: &PoolHandle);
}

impl<T> Pointable for T {
    #[cfg(not(feature = "pmcheck"))]
    const ALIGN: usize = mem::align_of::<T>();
    #[cfg(feature = "pmcheck")]
    const ALIGN: usize = 64;

    type Init = T;

    unsafe fn init(init: Self::Init, pool: &PoolHandle) -> usize {
        let ptr = pool.alloc::<T>();

        let t = ptr.deref_mut(pool);
        std::ptr::write(t as *mut T, init);
        ptr.into_offset()
    }

    unsafe fn deref(offset: usize, pool: &PoolHandle) -> &Self {
        PPtr::from(offset).deref(pool)
    }

    unsafe fn deref_mut(offset: usize, pool: &PoolHandle) -> &mut Self {
        PPtr::from(offset).deref_mut(pool)
    }

    unsafe fn drop(offset: usize, pool: &PoolHandle) {
        pool.free(PPtr::<T>::from(offset));
    }
}

/// Array with size.
///
/// # Memory layout
///
/// An array consisting of size and elements:
///
/// ```text
///          elements
///          |
///          |
/// ------------------------------------
/// | size | 0 | 1 | 2 | 3 | 4 | 5 | 6 |
/// ------------------------------------
/// ```
///
/// Its memory layout is different from that of `Box<[T]>` in that size is in the allocation (not
/// along with pointer as in `Box<[T]>`).
///
/// Elements are not present in the type, but they will be in the allocation.
/// ```
///
// TODO(crossbeam)(@jeehoonkang): once we bump the minimum required Rust version to 1.44 or newer, use
// [`alloc::alloc::Layout::extend`] instead.
#[repr(C)]
struct PArray<T> {
    /// The number of elements (not the number of bytes).
    len: usize,
    elements: [MaybeUninit<T>; 0],
}

impl<T> Pointable for [MaybeUninit<T>] {
    #[cfg(not(feature = "pmcheck"))]
    const ALIGN: usize = mem::align_of::<T>();
    #[cfg(feature = "pmcheck")]
    const ALIGN: usize = 64;

    type Init = usize;

    unsafe fn init(len: Self::Init, pool: &PoolHandle) -> usize {
        let size = mem::size_of::<PArray<T>>() + mem::size_of::<MaybeUninit<T>>() * len;
        let align = mem::align_of::<PArray<T>>();
        let layout = alloc::Layout::from_size_align(size, align).unwrap();
        let ptr = pool.alloc_layout::<PArray<T>>(layout);
        if ptr.is_null() {
            alloc::handle_alloc_error(layout);
        }
        let p = ptr.deref_mut(pool);
        p.len = len;
        ptr.into_offset()
    }

    unsafe fn deref(offset: usize, pool: &PoolHandle) -> &Self {
        let array = &*(PPtr::from(offset).deref(pool) as *const PArray<T>);
        slice::from_raw_parts(array.elements.as_ptr() as *const _, array.len)
    }

    unsafe fn deref_mut(offset: usize, pool: &PoolHandle) -> &mut Self {
        let array = &*(PPtr::from(offset).deref_mut(pool) as *mut PArray<T>);
        slice::from_raw_parts_mut(array.elements.as_ptr() as *mut _, array.len)
    }

    unsafe fn drop(offset: usize, pool: &PoolHandle) {
        let array = &*(PPtr::from(offset).deref_mut(pool) as *mut PArray<T>);
        let size = mem::size_of::<PArray<T>>() + mem::size_of::<MaybeUninit<T>>() * array.len;
        let align = mem::align_of::<PArray<T>>();
        let layout = alloc::Layout::from_size_align(size, align).unwrap();
        pool.free_layout(offset, layout)
    }
}

/// An atomic pointer that can be safely shared between threads.
///
/// The pointer must be properly aligned. Since it is aligned, a tag can be stored into the unused
/// least significant bits of the address. For example, the tag for a pointer to a sized type `T`
/// should be less than `(1 << mem::align_of::<T>().trailing_zeros())`.
///
/// Any method that loads the pointer must be passed a reference to a [`Guard`].
///
/// Crossbeam supports dynamically sized types.  See [`Pointable`] for details.
pub struct PAtomic<T: ?Sized + Pointable> {
    data: AtomicUsize,
    _marker: PhantomData<*mut T>,
}

unsafe impl<T: ?Sized + Pointable + Send + Sync> Send for PAtomic<T> {}
unsafe impl<T: ?Sized + Pointable + Send + Sync> Sync for PAtomic<T> {}

impl<T> PAtomic<T> {
    /// Allocates `value` on the persistent heap and returns a new atomic pointer pointing to it.
    ///
    /// # Examples
    ///
    /// ```
    /// # use memento::pmem::pool::*;
    /// # use memento::*;
    /// # use memento::test_utils::tests::get_dummy_handle;
    /// # let pool = get_dummy_handle(8 * 1024 * 1024 * 1024).unwrap();
    /// use memento::pepoch::PAtomic;
    ///
    /// // Assume there is PoolHandle, `pool`
    /// let a = PAtomic::new(1234, &pool);
    /// ```
    pub fn new(init: T, pool: &PoolHandle) -> PAtomic<T> {
        Self::init(init, pool)
    }
}

impl<T: ?Sized + Pointable> PAtomic<T> {
    /// Allocates `value` on the persistent heap and returns a new atomic pointer pointing to it.
    ///
    /// # Examples
    ///
    /// ```
    /// # use memento::pmem::pool::*;
    /// # use memento::*;
    /// # use memento::test_utils::tests::get_dummy_handle;
    /// # let pool = get_dummy_handle(8 * 1024 * 1024 * 1024).unwrap();
    /// use memento::pepoch::PAtomic;
    ///
    /// // Assume there is PoolHandle, `pool`
    /// let a = PAtomic::<i32>::init(1234, &pool);
    /// ```
    pub fn init(init: T::Init, pool: &PoolHandle) -> PAtomic<T> {
        Self::from(POwned::init(init, pool))
    }

    /// Returns a new atomic pointer pointing to the tagged pointer `data`.
    fn from_usize(data: usize) -> Self {
        Self {
            data: AtomicUsize::new(data),
            _marker: PhantomData,
        }
    }

    /// Returns a new null atomic pointer.
    ///
    /// # Examples
    ///
    /// ```
    /// use memento::pepoch::PAtomic;
    ///
    /// let a = PAtomic::<i32>::null();
    /// ```
    ///
    #[cfg_attr(all(feature = "nightly", not(crossbeam_loom)), const_fn::const_fn)]
    pub fn null() -> PAtomic<T> {
        let (_, _, _, _, offset, _) = decompose_tag::<T>(PPtr::<T>::null().into_offset());
        Self {
            data: AtomicUsize::new(offset),
            _marker: PhantomData,
        }
    }

    /// Loads a `PShared` from the atomic pointer.
    ///
    /// This method takes an [`Ordering`] argument which describes the memory ordering of this
    /// operation.
    ///
    /// # Examples
    ///
    /// ```
    /// # use memento::pmem::pool::*;
    /// # use memento::*;
    /// # use memento::test_utils::tests::get_dummy_handle;
    /// # let pool = get_dummy_handle(8 * 1024 * 1024 * 1024).unwrap();
    /// use memento::pepoch::{self as epoch, PAtomic};
    /// use std::sync::atomic::Ordering::SeqCst;
    ///
    /// // Assume there is PoolHandle, `pool`
    /// let a = PAtomic::new(1234, &pool);
    /// let guard = &epoch::pin();
    /// let p = a.load(SeqCst, guard);
    /// ```
    pub fn load<'g>(&self, ord: Ordering, _: &'g Guard) -> PShared<'g, T> {
        unsafe { PShared::from_usize(self.data.load(ord)) }
    }

    /// Loads a `PShared` from the atomic pointer using a "consume" memory ordering.
    ///
    /// This is similar to the "acquire" ordering, except that an ordering is
    /// only guaranteed with operations that "depend on" the result of the load.
    /// However consume loads are usually much faster than acquire loads on
    /// architectures with a weak memory model since they don't require memory
    /// fence instructions.
    ///
    /// The exact definition of "depend on" is a bit vague, but it works as you
    /// would expect in practice since a lot of software, especially the Linux
    /// kernel, rely on this behavior.
    ///
    /// # Examples
    ///
    /// ```
    /// # use memento::pmem::pool::*;
    /// # use memento::*;
    /// # use memento::test_utils::tests::get_dummy_handle;
    /// # let pool = get_dummy_handle(8 * 1024 * 1024 * 1024).unwrap();
    /// use memento::pepoch::{self as epoch, PAtomic};
    ///
    /// // Assume there is PoolHandle, `pool`
    /// let a = PAtomic::new(1234, &pool);
    /// let guard = &epoch::pin();
    /// let p = a.load_consume(guard);
    /// ```
    pub fn load_consume<'g>(&self, _: &'g Guard) -> PShared<'g, T> {
        unsafe { PShared::from_usize(self.data.load_consume()) }
    }

    /// Stores a `PShared` or `POwned` pointer into the atomic pointer.
    ///
    /// This method takes an [`Ordering`] argument which describes the memory ordering of this
    /// operation.
    ///
    /// # Examples
    ///
    /// ```
    /// # use memento::pmem::pool::*;
    /// # use memento::*;
    /// # use memento::test_utils::tests::get_dummy_handle;
    /// # let pool = get_dummy_handle(8 * 1024 * 1024 * 1024).unwrap();
    /// use memento::pepoch::{PAtomic, POwned, PShared};
    /// use std::sync::atomic::Ordering::SeqCst;
    ///
    /// // Assume there is PoolHandle, `pool`
    /// let a = PAtomic::new(1234, &pool);
    /// a.store(PShared::null(), SeqCst);
    /// a.store(POwned::new(1234, &pool), SeqCst);
    /// ```
    pub fn store<P: Pointer<T>>(&self, new: P, ord: Ordering) {
        self.data.store(new.into_usize(), ord);
    }

    /// Stores a `PShared` or `POwned` pointer into the atomic pointer, returning the previous
    /// `PShared`.
    ///
    /// This method takes an [`Ordering`] argument which describes the memory ordering of this
    /// operation.
    ///
    /// # Examples
    ///
    /// ```
    /// # use memento::pmem::pool::*;
    /// # use memento::*;
    /// # use memento::test_utils::tests::get_dummy_handle;
    /// # let pool = get_dummy_handle(8 * 1024 * 1024 * 1024).unwrap();
    /// use memento::pepoch::{self as epoch, PAtomic, PShared};
    /// use std::sync::atomic::Ordering::SeqCst;
    ///
    /// // Assume there is PoolHandle, `pool`
    /// let a = PAtomic::new(1234, &pool);
    /// let guard = &epoch::pin();
    /// let p = a.swap(PShared::null(), SeqCst, guard);
    /// ```
    pub fn swap<'g, P: Pointer<T>>(&self, new: P, ord: Ordering, _: &'g Guard) -> PShared<'g, T> {
        unsafe { PShared::from_usize(self.data.swap(new.into_usize(), ord)) }
    }

    /// Stores the pointer `new` (either `PShared` or `POwned`) into the atomic pointer if the current
    /// value is the same as `current`. The tag is also taken into account, so two pointers to the
    /// same object, but with different tags, will not be considered equal.
    ///
    /// The return value is a result indicating whether the new pointer was written. On success the
    /// pointer that was written is returned. On failure the actual current value and `new` are
    /// returned.
    ///
    /// This method takes two `Ordering` arguments to describe the memory
    /// ordering of this operation. `success` describes the required ordering for the
    /// read-modify-write operation that takes place if the comparison with `current` succeeds.
    /// `failure` describes the required ordering for the load operation that takes place when
    /// the comparison fails. Using `Acquire` as success ordering makes the store part
    /// of this operation `Relaxed`, and using `Release` makes the successful load
    /// `Relaxed`. The failure ordering can only be `SeqCst`, `Acquire` or `Relaxed`
    /// and must be equivalent to or weaker than the success ordering.
    ///
    /// # Examples
    ///
    /// ```
    /// # use memento::pmem::pool::*;
    /// # use memento::*;
    /// # use memento::test_utils::tests::get_dummy_handle;
    /// # let pool = get_dummy_handle(8 * 1024 * 1024 * 1024).unwrap();
    /// use memento::pepoch::{self as epoch, PAtomic, POwned, PShared};
    /// use std::sync::atomic::Ordering::SeqCst;
    ///
    /// // Assume there is PoolHandle, `pool`
    /// let a = PAtomic::new(1234, &pool);
    ///
    /// let guard = &epoch::pin();
    /// let curr = a.load(SeqCst, guard);
    /// let res1 = a.compare_exchange(curr, PShared::null(), SeqCst, SeqCst, guard);
    /// let res2 = a.compare_exchange(curr, POwned::new(5678, &pool), SeqCst, SeqCst, guard);
    /// ```
    pub fn compare_exchange<'g, P>(
        &self,
        current: PShared<'_, T>,
        new: P,
        success: Ordering,
        failure: Ordering,
        _: &'g Guard,
    ) -> Result<PShared<'g, T>, CompareExchangeError<'g, T, P>>
    where
        P: Pointer<T>,
    {
        let new = new.into_usize();
        self.data
            .compare_exchange(current.into_usize(), new, success, failure)
            .map(|_| unsafe { PShared::from_usize(new) })
            .map_err(|current| unsafe {
                CompareExchangeError {
                    current: PShared::from_usize(current),
                    new: P::from_usize(new),
                }
            })
    }

    /// Stores the pointer `new` (either `PShared` or `POwned`) into the atomic pointer if the current
    /// value is the same as `current`. The tag is also taken into account, so two pointers to the
    /// same object, but with different tags, will not be considered equal.
    ///
    /// Unlike [`compare_exchange`], this method is allowed to spuriously fail even when comparison
    /// succeeds, which can result in more efficient code on some platforms.  The return value is a
    /// result indicating whether the new pointer was written. On success the pointer that was
    /// written is returned. On failure the actual current value and `new` are returned.
    ///
    /// This method takes two `Ordering` arguments to describe the memory
    /// ordering of this operation. `success` describes the required ordering for the
    /// read-modify-write operation that takes place if the comparison with `current` succeeds.
    /// `failure` describes the required ordering for the load operation that takes place when
    /// the comparison fails. Using `Acquire` as success ordering makes the store part
    /// of this operation `Relaxed`, and using `Release` makes the successful load
    /// `Relaxed`. The failure ordering can only be `SeqCst`, `Acquire` or `Relaxed`
    /// and must be equivalent to or weaker than the success ordering.
    ///
    /// [`compare_exchange`]: PAtomic::compare_exchange
    ///
    /// # Examples
    ///
    /// ```
    /// # use memento::pmem::pool::*;
    /// # use memento::*;
    /// # use memento::test_utils::tests::get_dummy_handle;
    /// # let pool = get_dummy_handle(8 * 1024 * 1024 * 1024).unwrap();
    /// use memento::pepoch::{self as epoch, PAtomic, POwned, PShared};
    /// use std::sync::atomic::Ordering::SeqCst;
    ///
    /// // Assume there is PoolHandle, `pool`
    /// let a = PAtomic::new(1234, &pool);
    /// let guard = &epoch::pin();
    ///
    /// let mut new = POwned::new(5678, &pool);
    /// let mut ptr = a.load(SeqCst, guard);
    /// loop {
    ///     match a.compare_exchange_weak(ptr, new, SeqCst, SeqCst, guard) {
    ///         Ok(p) => {
    ///             ptr = p;
    ///             break;
    ///         }
    ///         Err(err) => {
    ///             ptr = err.current;
    ///             new = err.new;
    ///         }
    ///     }
    /// }
    ///
    /// let mut curr = a.load(SeqCst, guard);
    /// loop {
    ///     match a.compare_exchange_weak(curr, PShared::null(), SeqCst, SeqCst, guard) {
    ///         Ok(_) => break,
    ///         Err(err) => curr = err.current,
    ///     }
    /// }
    /// ```
    pub fn compare_exchange_weak<'g, P>(
        &self,
        current: PShared<'_, T>,
        new: P,
        success: Ordering,
        failure: Ordering,
        _: &'g Guard,
    ) -> Result<PShared<'g, T>, CompareExchangeError<'g, T, P>>
    where
        P: Pointer<T>,
    {
        let new = new.into_usize();
        self.data
            .compare_exchange_weak(current.into_usize(), new, success, failure)
            .map(|_| unsafe { PShared::from_usize(new) })
            .map_err(|current| unsafe {
                CompareExchangeError {
                    current: PShared::from_usize(current),
                    new: P::from_usize(new),
                }
            })
    }

    /// Fetches the pointer, and then applies a function to it that returns a new value.
    /// Returns a `Result` of `Ok(previous_value)` if the function returned `Some`, else `Err(_)`.
    ///
    /// Note that the given function may be called multiple times if the value has been changed by
    /// other threads in the meantime, as long as the function returns `Some(_)`, but the function
    /// will have been applied only once to the stored value.
    ///
    /// `fetch_update` takes two [`Ordering`] arguments to describe the memory
    /// ordering of this operation. The first describes the required ordering for
    /// when the operation finally succeeds while the second describes the
    /// required ordering for loads. These correspond to the success and failure
    /// orderings of [`PAtomic::compare_exchange`] respectively.
    ///
    /// Using [`Acquire`] as success ordering makes the store part of this
    /// operation [`Relaxed`], and using [`Release`] makes the final successful
    /// load [`Relaxed`]. The (failed) load ordering can only be [`SeqCst`],
    /// [`Acquire`] or [`Relaxed`] and must be equivalent to or weaker than the
    /// success ordering.
    ///
    /// [`Relaxed`]: Ordering::Relaxed
    /// [`Acquire`]: Ordering::Acquire
    /// [`Release`]: Ordering::Release
    /// [`SeqCst`]: Ordering::SeqCst
    ///
    /// # Examples
    ///
    /// ```
    /// # use memento::pmem::pool::*;
    /// # use memento::*;
    /// # use memento::test_utils::tests::get_dummy_handle;
    /// # let pool = get_dummy_handle(8 * 1024 * 1024 * 1024).unwrap();
    /// use memento::pepoch::{self as epoch, PAtomic};
    /// use std::sync::atomic::Ordering::SeqCst;
    ///
    /// // Assume there is PoolHandle, `pool`
    /// let a = PAtomic::new(1234, &pool);
    /// let guard = &epoch::pin();
    ///
    /// let res1 = a.fetch_update(SeqCst, SeqCst, guard, |x| Some(x.with_tag(1)));
    /// assert!(res1.is_ok());
    ///
    /// let res2 = a.fetch_update(SeqCst, SeqCst, guard, |x| None);
    /// assert!(res2.is_err());
    /// ```
    pub fn fetch_update<'g, F>(
        &self,
        set_order: Ordering,
        fail_order: Ordering,
        guard: &'g Guard,
        mut func: F,
    ) -> Result<PShared<'g, T>, PShared<'g, T>>
    where
        F: FnMut(PShared<'g, T>) -> Option<PShared<'g, T>>,
    {
        let mut prev = self.load(fail_order, guard);
        while let Some(next) = func(prev) {
            match self.compare_exchange_weak(prev, next, set_order, fail_order, guard) {
                Ok(shared) => return Ok(shared),
                Err(next_prev) => prev = next_prev.current,
            }
        }
        Err(prev)
    }

    /// Stores the pointer `new` (either `PShared` or `POwned`) into the atomic pointer if the current
    /// value is the same as `current`. The tag is also taken into account, so two pointers to the
    /// same object, but with different tags, will not be considered equal.
    ///
    /// The return value is a result indicating whether the new pointer was written. On success the
    /// pointer that was written is returned. On failure the actual current value and `new` are
    /// returned.
    ///
    /// This method takes a [`CompareAndSetOrdering`] argument which describes the memory
    /// ordering of this operation.
    ///
    /// # Migrating to `compare_exchange`
    ///
    /// `compare_and_set` is equivalent to `compare_exchange` with the following mapping for
    /// memory orderings:
    ///
    /// Original | Success | Failure
    /// -------- | ------- | -------
    /// Relaxed  | Relaxed | Relaxed
    /// Acquire  | Acquire | Acquire
    /// Release  | Release | Relaxed
    /// AcqRel   | AcqRel  | Acquire
    /// SeqCst   | SeqCst  | SeqCst
    ///
    /// # Examples
    ///
    /// ```
    /// # #![allow(deprecated)]
    /// # use memento::pmem::pool::*;
    /// # use memento::*;
    /// # use memento::test_utils::tests::get_dummy_handle;
    /// # let pool = get_dummy_handle(8 * 1024 * 1024 * 1024).unwrap();
    /// use memento::pepoch::{self as epoch, PAtomic, POwned, PShared};
    /// use std::sync::atomic::Ordering::SeqCst;
    ///
    /// // Assume there is PoolHandle, `pool`
    /// let a = PAtomic::new(1234, &pool);
    ///
    /// let guard = &epoch::pin();
    /// let curr = a.load(SeqCst, guard);
    /// let res1 = a.compare_and_set(curr, PShared::null(), SeqCst, guard);
    /// let res2 = a.compare_and_set(curr, POwned::new(5678, &pool), SeqCst, guard);
    /// ```
    // TODO(crossbeam): remove in the next major version.
    #[allow(deprecated)]
    #[deprecated(note = "Use `compare_exchange` instead")]
    pub fn compare_and_set<'g, O, P>(
        &self,
        current: PShared<'_, T>,
        new: P,
        ord: O,
        guard: &'g Guard,
    ) -> Result<PShared<'g, T>, CompareAndSetError<'g, T, P>>
    where
        O: CompareAndSetOrdering,
        P: Pointer<T>,
    {
        self.compare_exchange(current, new, ord.success(), ord.failure(), guard)
    }

    /// Stores the pointer `new` (either `PShared` or `POwned`) into the atomic pointer if the current
    /// value is the same as `current`. The tag is also taken into account, so two pointers to the
    /// same object, but with different tags, will not be considered equal.
    ///
    /// Unlike [`compare_and_set`], this method is allowed to spuriously fail even when comparison
    /// succeeds, which can result in more efficient code on some platforms.  The return value is a
    /// result indicating whether the new pointer was written. On success the pointer that was
    /// written is returned. On failure the actual current value and `new` are returned.
    ///
    /// This method takes a [`CompareAndSetOrdering`] argument which describes the memory
    /// ordering of this operation.
    ///
    /// [`compare_and_set`]: PAtomic::compare_and_set
    ///
    /// # Migrating to `compare_exchange_weak`
    ///
    /// `compare_and_set_weak` is equivalent to `compare_exchange_weak` with the following mapping for
    /// memory orderings:
    ///
    /// Original | Success | Failure
    /// -------- | ------- | -------
    /// Relaxed  | Relaxed | Relaxed
    /// Acquire  | Acquire | Acquire
    /// Release  | Release | Relaxed
    /// AcqRel   | AcqRel  | Acquire
    /// SeqCst   | SeqCst  | SeqCst
    ///
    /// # Examples
    ///
    /// ```
    /// # #![allow(deprecated)]
    /// # use memento::pmem::pool::*;
    /// # use memento::*;
    /// # use memento::test_utils::tests::get_dummy_handle;
    /// # let pool = get_dummy_handle(8 * 1024 * 1024 * 1024).unwrap();
    /// use memento::pepoch::{self as epoch, PAtomic, POwned, PShared};
    /// use std::sync::atomic::Ordering::SeqCst;
    ///
    /// // Assume there is PoolHandle, `pool`
    /// let a = PAtomic::new(1234, &pool);
    /// let guard = &epoch::pin();
    ///
    /// let mut new = POwned::new(5678, &pool);
    /// let mut ptr = a.load(SeqCst, guard);
    /// loop {
    ///     match a.compare_and_set_weak(ptr, new, SeqCst, guard) {
    ///         Ok(p) => {
    ///             ptr = p;
    ///             break;
    ///         }
    ///         Err(err) => {
    ///             ptr = err.current;
    ///             new = err.new;
    ///         }
    ///     }
    /// }
    ///
    /// let mut curr = a.load(SeqCst, guard);
    /// loop {
    ///     match a.compare_and_set_weak(curr, PShared::null(), SeqCst, guard) {
    ///         Ok(_) => break,
    ///         Err(err) => curr = err.current,
    ///     }
    /// }
    /// ```
    // TODO(crossbeam): remove in the next major version.
    #[allow(deprecated)]
    #[deprecated(note = "Use `compare_exchange_weak` instead")]
    pub fn compare_and_set_weak<'g, O, P>(
        &self,
        current: PShared<'_, T>,
        new: P,
        ord: O,
        guard: &'g Guard,
    ) -> Result<PShared<'g, T>, CompareAndSetError<'g, T, P>>
    where
        O: CompareAndSetOrdering,
        P: Pointer<T>,
    {
        self.compare_exchange_weak(current, new, ord.success(), ord.failure(), guard)
    }

    /// Bitwise "and" with the current tag.
    ///
    /// Performs a bitwise "and" operation on the current tag and the argument `val`, and sets the
    /// new tag to the result. Returns the previous pointer.
    ///
    /// This method takes an [`Ordering`] argument which describes the memory ordering of this
    /// operation.
    ///
    /// # Examples
    ///
    /// ```
    /// # use memento::pmem::pool::*;
    /// # use memento::*;
    /// # use memento::test_utils::tests::get_dummy_handle;
    /// # let pool = get_dummy_handle(8 * 1024 * 1024 * 1024).unwrap();
    /// use memento::pepoch::{self as epoch, PAtomic, PShared};
    /// use std::sync::atomic::Ordering::SeqCst;
    ///
    /// // Assume there is PoolHandle, `pool`
    /// let a = PAtomic::<i32>::from(PShared::null().with_tag(3));
    /// let guard = &epoch::pin();
    /// assert_eq!(a.fetch_and(2, SeqCst, guard).tag(), 3);
    /// assert_eq!(a.load(SeqCst, guard).tag(), 2);
    /// ```
    pub fn fetch_and<'g>(&self, val: usize, ord: Ordering, _: &'g Guard) -> PShared<'g, T> {
        unsafe { PShared::from_usize(self.data.fetch_and(val | !low_bits::<T>(), ord)) }
    }

    /// Bitwise "or" with the current tag.
    ///
    /// Performs a bitwise "or" operation on the current tag and the argument `val`, and sets the
    /// new tag to the result. Returns the previous pointer.
    ///
    /// This method takes an [`Ordering`] argument which describes the memory ordering of this
    /// operation.
    ///
    /// # Examples
    ///
    /// ```
    /// # use memento::pmem::pool::*;
    /// # use memento::*;
    /// # use memento::test_utils::tests::get_dummy_handle;
    /// # let pool = get_dummy_handle(8 * 1024 * 1024 * 1024).unwrap();
    /// use memento::pepoch::{self as epoch, PAtomic, PShared};
    /// use std::sync::atomic::Ordering::SeqCst;
    /// // Assume there is PoolHandle, `pool`
    /// let a = PAtomic::<i32>::from(PShared::null().with_tag(1));
    /// let guard = &epoch::pin();
    /// assert_eq!(a.fetch_or(2, SeqCst, guard).tag(), 1);
    /// assert_eq!(a.load(SeqCst, guard).tag(), 3);
    /// ```
    pub fn fetch_or<'g>(&self, val: usize, ord: Ordering, _: &'g Guard) -> PShared<'g, T> {
        unsafe { PShared::from_usize(self.data.fetch_or(val & low_bits::<T>(), ord)) }
    }

    /// Bitwise "xor" with the current tag.
    ///
    /// Performs a bitwise "xor" operation on the current tag and the argument `val`, and sets the
    /// new tag to the result. Returns the previous pointer.
    ///
    /// This method takes an [`Ordering`] argument which describes the memory ordering of this
    /// operation.
    ///
    /// # Examples
    ///
    /// ```
    /// # use memento::pmem::pool::*;
    /// # use memento::*;
    /// # use memento::test_utils::tests::get_dummy_handle;
    /// # let pool = get_dummy_handle(8 * 1024 * 1024 * 1024).unwrap();
    /// use memento::pepoch::{self as epoch, PAtomic, PShared};
    /// use std::sync::atomic::Ordering::SeqCst;
    ///
    /// // Assume there is PoolHandle, `pool`
    /// let a = PAtomic::<i32>::from(PShared::null().with_tag(1));
    /// let guard = &epoch::pin();
    /// assert_eq!(a.fetch_xor(3, SeqCst, guard).tag(), 1);
    /// assert_eq!(a.load(SeqCst, guard).tag(), 2);
    /// ```
    pub fn fetch_xor<'g>(&self, val: usize, ord: Ordering, _: &'g Guard) -> PShared<'g, T> {
        unsafe { PShared::from_usize(self.data.fetch_xor(val & low_bits::<T>(), ord)) }
    }

    /// Takes ownership of the pointee.
    ///
    /// This consumes the atomic and converts it into [`POwned`]. As [`PAtomic`] doesn't have a
    /// destructor and doesn't drop the pointee while [`POwned`] does, this is suitable for
    /// destructors of data structures.
    ///
    /// # Panics
    ///
    /// Panics if this pointer is null, but only in debug mode.
    ///
    /// # Safety
    ///
    /// This method may be called only if the pointer is valid and nobody else is holding a
    /// reference to the same object.
    ///
    /// # Examples
    ///
    /// ```rust
    /// # use std::mem;
    /// # use memento::pepoch::PAtomic;
    /// struct DataStructure {
    ///     ptr: PAtomic<usize>,
    /// }
    ///
    /// impl Drop for DataStructure {
    ///     fn drop(&mut self) {
    ///         // By now the DataStructure lives only in our thread and we are sure we don't hold
    ///         // any Shared or & to it ourselves.
    ///         unsafe {
    ///             drop(mem::replace(&mut self.ptr, PAtomic::null()).into_owned());
    ///         }
    ///     }
    /// }
    /// ```
    pub unsafe fn into_owned(self) -> POwned<T> {
        #[cfg(crossbeam_loom)]
        {
            // FIXME(crossbeam): loom does not yet support into_inner, so we use unsync_load for now,
            // which should have the same synchronization properties:
            // https://github.com/tokio-rs/loom/issues/117
            POwned::from_usize(self.data.unsync_load())
        }
        #[cfg(not(crossbeam_loom))]
        {
            POwned::from_usize(self.data.into_inner())
        }
    }

    /// Format
    pub fn fmt(&self, f: &mut fmt::Formatter<'_>, pool: &PoolHandle) -> fmt::Result {
        let data = self.data.load(Ordering::SeqCst);
        let (_, _, _, _, offset, _) = decompose_tag::<T>(data);
        fmt::Pointer::fmt(&(unsafe { T::deref(offset, pool) as *const _ }), f)
    }
}

impl<T: ?Sized + Pointable> fmt::Debug for PAtomic<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let data = self.data.load(Ordering::SeqCst);
        let (aux_bit, desc_bit, tid, htag, offset, ltag) = decompose_tag::<T>(data);

        f.debug_struct("Atomic")
            .field("aux bit", &aux_bit)
            .field("desc bit", &desc_bit)
            .field("tid", &tid)
            .field("high tag", &htag)
            .field("offset", &offset)
            .field("low tag", &ltag)
            .finish()
    }
}

impl<T: ?Sized + Pointable> Clone for PAtomic<T> {
    /// Returns a copy of the atomic value.
    ///
    /// Note that a `Relaxed` load is used here. If you need synchronization, use it with other
    /// atomics or fences.
    fn clone(&self) -> Self {
        let data = self.data.load(Ordering::Relaxed);
        PAtomic::from_usize(data)
    }
}

impl<T: ?Sized + Pointable> Default for PAtomic<T> {
    fn default() -> Self {
        PAtomic::null()
    }
}

impl<T: ?Sized + Pointable> From<POwned<T>> for PAtomic<T> {
    /// Returns a new atomic pointer pointing to `POwned`.
    ///
    /// # Examples
    ///
    /// ```
    /// # use memento::pmem::pool::*;
    /// # use memento::*;
    /// # use memento::test_utils::tests::get_dummy_handle;
    /// # let pool = get_dummy_handle(8 * 1024 * 1024 * 1024).unwrap();
    /// use memento::pepoch::{PAtomic, POwned};
    ///
    /// // Assume there is PoolHandle, `pool`
    /// let a = PAtomic::<i32>::from(POwned::new(1234, &pool));
    /// ```
    fn from(owned: POwned<T>) -> Self {
        let data = owned.data;
        mem::forget(owned);
        Self::from_usize(data)
    }
}

impl<'g, T: ?Sized + Pointable> From<PShared<'g, T>> for PAtomic<T> {
    /// Returns a new atomic pointer pointing to `ptr`.
    ///
    /// # Examples
    ///
    /// ```
    /// use memento::pepoch::{PAtomic, PShared};
    ///
    /// let a = PAtomic::<i32>::from(PShared::<i32>::null());
    /// ```
    fn from(ptr: PShared<'g, T>) -> Self {
        Self::from_usize(ptr.data)
    }
}

impl<T> From<PPtr<T>> for PAtomic<T> {
    /// Returns a new atomic pointer pointing to `ptr`.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::ptr;
    /// use memento::pmem::ptr::PPtr;
    /// use memento::pepoch::PAtomic;
    ///
    /// let a = PAtomic::<i32>::from(PPtr::<i32>::null());
    /// ```
    fn from(ptr: PPtr<T>) -> Self {
        Self::from_usize(ptr.into_offset())
    }
}

impl<T: Collectable> Collectable for PAtomic<T> {
    fn filter(s: &mut Self, tid: usize, gc: &mut GarbageCollection, pool: &mut PoolHandle) {
        let guard = unsafe { unprotected() };

        let mut ptr = s.load(Ordering::Relaxed, guard);
        if !ptr.is_null() {
            let t_ref = unsafe { ptr.deref_mut(pool) };
            T::mark(t_ref, tid, gc);
        }
    }
}

impl<T: Collectable> PDefault for PAtomic<T> {
    fn pdefault(_: &Handle) -> Self {
        Default::default()
    }
}
/// A trait for either `POwned` or `PShared` pointers.
pub trait Pointer<T: ?Sized + Pointable> {
    /// Returns the machine representation of the pointer.
    fn into_usize(self) -> usize;

    /// Returns a new pointer pointing to the tagged pointer `data`.
    ///
    /// # Safety
    ///
    /// The given `data` should have been created by `Pointer::into_usize()`, and one `data` should
    /// not be converted back by `Pointer::from_usize()` multiple times.
    unsafe fn from_usize(data: usize) -> Self;
}

/// An owned heap-allocated object.
///
/// This type is very similar to `Box<T>`.
///
/// The pointer must be properly aligned. Since it is aligned, a tag can be stored into the unused
/// least significant bits of the address.
pub struct POwned<T: ?Sized + Pointable> {
    data: usize,
    _marker: PhantomData<T>,
}

impl<T: ?Sized + Pointable> Pointer<T> for POwned<T> {
    #[inline]
    fn into_usize(self) -> usize {
        let data = self.data;
        mem::forget(self);
        data
    }

    /// Returns a new pointer pointing to the tagged pointer `data`.
    ///
    /// # Panics
    ///
    /// Panics if the data is zero in debug mode.
    #[inline]
    unsafe fn from_usize(data: usize) -> Self {
        debug_assert!(data != 0, "converting zero into `POwned`");
        POwned {
            data,
            _marker: PhantomData,
        }
    }
}

impl<T> POwned<T> {
    /// Returns a new owned pointer pointing to `ptr`.
    ///
    /// This function is unsafe because improper use may lead to memory problems. Argument `ptr`
    /// must be a valid pointer. Also, a double-free may occur if the function is called twice on
    /// the same ptr pointer.
    ///
    /// # Panics
    ///
    /// Panics if `ptr` is not properly aligned.
    ///
    /// # Safety
    ///
    /// The given `ptr` should have been derived from `POwned`, and one `ptr` should not be converted
    /// back by `POwned::from_ptr()` multiple times.
    ///
    /// # Examples
    ///
    /// ```
    /// # use memento::pmem::pool::*;
    /// # use memento::*;
    /// # use memento::test_utils::tests::get_dummy_handle;
    /// # let pool = get_dummy_handle(8 * 1024 * 1024 * 1024).unwrap();
    /// use memento::pmem::ptr::PPtr;
    /// use memento::pepoch::POwned;
    ///
    /// // Assume there is PoolHandle, `pool`
    /// let mut ptr = pool.alloc::<usize>();
    /// let o = unsafe { POwned::from_ptr(ptr) };
    /// ```
    pub unsafe fn from_ptr(ptr: PPtr<T>) -> POwned<T> {
        let offset = ptr.into_offset();
        ensure_aligned::<T>(offset);
        Self::from_usize(offset)
    }

    /// Allocates `value` on the persistent heap and returns a new owned pointer pointing to it.
    ///
    /// # Examples
    ///
    /// ```
    /// # use memento::pmem::pool::*;
    /// # use memento::*;
    /// # use memento::test_utils::tests::get_dummy_handle;
    /// # let pool = get_dummy_handle(8 * 1024 * 1024 * 1024).unwrap();
    /// use memento::pepoch::POwned;
    ///
    /// // Assume there is PoolHandle, `pool`
    /// let o = POwned::new(1234, &pool);
    /// ```
    pub fn new(init: T, pool: &PoolHandle) -> POwned<T> {
        Self::init(init, pool)
    }
}

impl<T: ?Sized + Pointable> POwned<T> {
    /// Allocates `value` on the persistent heap and returns a new owned pointer pointing to it.
    ///
    /// # Examples
    ///
    /// ```
    /// # use memento::pmem::pool::*;
    /// # use memento::*;
    /// # use memento::test_utils::tests::get_dummy_handle;
    /// # let pool = get_dummy_handle(8 * 1024 * 1024 * 1024).unwrap();
    /// use memento::pepoch::POwned;
    ///
    /// // Assume there is PoolHandle, `pool`
    /// let o = POwned::<i32>::init(1234, &pool);
    /// ```
    pub fn init(init: T::Init, pool: &PoolHandle) -> POwned<T> {
        unsafe { Self::from_usize(T::init(init, pool)) }
    }

    /// Converts the owned pointer into a [`PShared`].
    ///
    /// # Examples
    ///
    /// ```
    /// # use memento::pmem::pool::*;
    /// # use memento::*;
    /// # use memento::test_utils::tests::get_dummy_handle;
    /// # let pool = get_dummy_handle(8 * 1024 * 1024 * 1024).unwrap();
    /// use memento::pepoch::{self as epoch, POwned};
    ///
    /// // Assume there is PoolHandle, `pool`
    /// let o = POwned::new(1234, &pool);
    /// let guard = &epoch::pin();
    /// let p = o.into_shared(guard);
    /// ```
    #[allow(clippy::needless_lifetimes)]
    pub fn into_shared<'g>(self, _: &'g Guard) -> PShared<'g, T> {
        unsafe { PShared::from_usize(self.into_usize()) }
    }

    /// Returns the tag stored within the pointer.
    ///
    /// # Examples
    ///
    /// ```
    /// # use memento::pmem::pool::*;
    /// # use memento::*;
    /// # use memento::test_utils::tests::get_dummy_handle;
    /// # let pool = get_dummy_handle(8 * 1024 * 1024 * 1024).unwrap();
    /// use memento::pepoch::POwned;
    ///
    /// // Assume there is PoolHandle, `pool`
    /// assert_eq!(POwned::new(1234, &pool).tag(), 0);
    /// ```
    pub fn tag(&self) -> usize {
        let (_, _, _, _, _, tag) = decompose_tag::<T>(self.data);
        tag
    }

    /// get aux_bit
    pub fn aux_bit(&self) -> usize {
        let (aux_bit, _, _, _, _, _) = decompose_tag::<T>(self.data);
        aux_bit
    }

    /// get desc_bit
    pub fn desc_bit(&self) -> usize {
        let (_, desc_bit, _, _, _, _) = decompose_tag::<T>(self.data);
        desc_bit
    }

    /// get tid
    pub fn tid(&self) -> usize {
        let (_, _, tid, _, _, _) = decompose_tag::<T>(self.data);
        tid
    }

    /// get high_tag
    pub fn high_tag(&self) -> usize {
        let (_, _, _, tag, _, _) = decompose_tag::<T>(self.data);
        tag
    }

    /// Returns the same pointer, but tagged with `tag`. `tag` is truncated to be fit into the
    /// unused bits of the pointer to `T`.
    ///
    /// # Examples
    ///
    /// ```
    /// # use memento::pmem::pool::*;
    /// # use memento::*;
    /// # use memento::test_utils::tests::get_dummy_handle;
    /// # let pool = get_dummy_handle(8 * 1024 * 1024 * 1024).unwrap();
    /// use memento::pepoch::POwned;
    ///
    /// // Assume there is PoolHandle, `pool`
    /// let o = POwned::new(0u64, &pool);
    /// assert_eq!(o.tag(), 0);
    /// let o = o.with_tag(2);
    /// assert_eq!(o.tag(), 2);
    /// ```
    pub fn with_tag(self, tag: usize) -> POwned<T> {
        let data = self.into_usize();
        unsafe { Self::from_usize(compose_tag::<T>(data, tag)) }
    }

    /// Set aux bit
    pub fn with_aux_bit(self, aux_bit: usize) -> POwned<T> {
        let data = self.into_usize();
        unsafe { Self::from_usize(compose_aux_bit(aux_bit, data)) }
    }

    /// Set descripot bit
    pub fn with_desc_bit(&self, desc_bit: usize) -> POwned<T> {
        unsafe { Self::from_usize(compose_desc_bit(desc_bit, self.data)) }
    }

    /// Set tid
    pub fn with_tid(self, tid: usize) -> POwned<T> {
        let data = self.into_usize();
        unsafe { Self::from_usize(compose_tid(tid, data)) }
    }

    /// Returns the same pointer, but tagged with `tag`. `tag` is truncated to be fit into the
    /// unused high bits of the pointer to `T`.
    pub fn with_high_tag(self, tag: usize) -> POwned<T> {
        let data = self.into_usize();
        unsafe { Self::from_usize(compose_high_tag(tag, data)) }
    }

    /// deref absolute addr based on pool
    ///
    /// # Safety
    ///
    /// pool should be correct
    pub unsafe fn deref<'a>(&self, pool: &'a PoolHandle) -> &'a T {
        let (_, _, _, _, offset, _) = decompose_tag::<T>(self.data);
        T::deref(offset, pool)
    }

    /// deref absoulte addr based on pool
    ///
    /// # Safety
    ///
    /// pool should be correct
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn deref_mut<'a>(&mut self, pool: &'a PoolHandle) -> &'a mut T {
        let (_, _, _, _, offset, _) = decompose_tag::<T>(self.data);
        T::deref_mut(offset, pool)
    }

    /// borrow abolsulte addr based on pool
    ///
    /// # Safety
    ///
    /// pool should be correct
    pub unsafe fn borrow<'a>(&self, pool: &'a PoolHandle) -> &'a T {
        self.deref(pool)
    }

    /// borrow_mut abolsulte addr based on pool
    ///
    /// # Safety
    ///
    /// pool should be correct
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn borrow_mut<'a>(&mut self, pool: &'a PoolHandle) -> &'a mut T {
        self.deref_mut(pool)
    }

    // as_ref absolute addr based on pool
    /// as_ref
    ///
    /// # Safety
    ///
    /// pool should be correct
    pub unsafe fn as_ref<'a>(&self, pool: &'a PoolHandle) -> &'a T {
        self.deref(pool)
    }

    /// as_mut absolute addr based on pool
    ///
    /// # Safety
    ///
    /// pool should be correct
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn as_mut<'a>(&mut self, pool: &'a PoolHandle) -> &'a mut T {
        self.deref_mut(pool)
    }
}

impl<T: ?Sized + Pointable> Drop for POwned<T> {
    fn drop(&mut self) {
        let (_, _, _, _, offset, _) = decompose_tag::<T>(self.data);
        unsafe {
            T::drop(offset, global_pool().unwrap());
        }
    }
}

impl<T: ?Sized + Pointable> fmt::Debug for POwned<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (aux_bit, desc_bit, tid, high_tag, offset, tag) = decompose_tag::<T>(self.data);

        f.debug_struct("Owned")
            .field("aux bit", &aux_bit)
            .field("desc bit", &desc_bit)
            .field("tid", &tid)
            .field("high tag", &high_tag)
            .field("offset", &offset)
            .field("tag", &tag)
            .finish()
    }
}

impl<T: Clone> POwned<T> {
    /// Clone
    pub fn clone(&self, pool: &PoolHandle) -> Self {
        POwned::new(unsafe { self.deref(pool) }.clone(), pool).with_tag(self.tag())
    }
}

impl<T: Collectable> Collectable for POwned<T> {
    fn filter(s: &mut Self, tid: usize, gc: &mut GarbageCollection, pool: &mut PoolHandle) {
        let item = unsafe { (*s).deref_mut(pool) };
        T::mark(item, tid, gc);
    }
}

// impl<T> From<Box<T>> for Owned<T> {
//     /// Returns a new owned pointer pointing to `b`.
//     ///
//     /// # Panics
//     ///
//     /// Panics if the pointer (the `Box`) is not properly aligned.
//     ///
//     /// # Examples
//     ///
//     /// ```
//     /// use crossbeam_epoch::Owned;
//     ///
//     /// let o = unsafe { Owned::from_raw(Box::into_raw(Box::new(1234))) };
//     /// ```
//     fn from(b: Box<T>) -> Self {
//         unsafe { Self::from_raw(Box::into_raw(b)) }
//     }
// }
/// A pointer to an object protected by the epoch GC.
///
/// The pointer is valid for use only during the lifetime `'g`.
///
/// The pointer must be properly aligned. Since it is aligned, a tag can be stored into the unused
/// least significant bits of the address.
pub struct PShared<'g, T: 'g + ?Sized + Pointable> {
    data: usize,
    _marker: PhantomData<(&'g (), *const T)>,
}

impl<T: ?Sized + Pointable> Clone for PShared<'_, T> {
    fn clone(&self) -> Self {
        Self {
            data: self.data,
            _marker: PhantomData,
        }
    }
}

impl<T: ?Sized + Pointable> Copy for PShared<'_, T> {}

impl<T: ?Sized + Pointable> Pointer<T> for PShared<'_, T> {
    #[inline]
    fn into_usize(self) -> usize {
        self.data
    }

    #[inline]
    unsafe fn from_usize(data: usize) -> Self {
        PShared {
            data,
            _marker: PhantomData,
        }
    }
}

impl<T> PShared<'_, T> {
    /// Converts the shared pointer to a raw persistent pointer (without the tag).
    ///
    /// # Examples
    ///
    /// ```
    /// # use memento::pmem::pool::*;
    /// # use memento::*;
    /// # use memento::test_utils::tests::get_dummy_handle;
    /// # let pool = get_dummy_handle(8 * 1024 * 1024 * 1024).unwrap();
    /// use memento::pmem::ptr::PPtr;
    /// use memento::pepoch::{self as epoch, PAtomic, POwned};
    /// use std::sync::atomic::Ordering::SeqCst;
    ///
    /// // Assume there is PoolHandle, `pool`
    /// let o = POwned::new(1234, &pool);
    /// let ptr = PPtr::from(unsafe { o.deref(&pool) as *const _ as usize } - pool.start());
    /// let a = PAtomic::from(o);
    ///
    /// let guard = &epoch::pin();
    /// let p = a.load(SeqCst, guard);
    /// assert_eq!(p.as_ptr(), ptr);
    /// ```
    #[allow(clippy::trivially_copy_pass_by_ref)]
    // TODO: change this into offset()
    pub fn as_ptr(&self) -> PPtr<T> {
        let (_, _, _, _, offset, _) = decompose_tag::<T>(self.data);
        PPtr::from(offset)
    }
}

impl<'g, T: ?Sized + Pointable> PShared<'g, T> {
    /// Returns a new null pointer.
    ///
    /// # Examples
    ///
    /// ```
    /// use memento::pepoch::PShared;
    ///
    /// let p = PShared::<i32>::null();
    /// assert!(p.is_null());
    /// ```
    pub fn null() -> PShared<'g, T> {
        let (_, _, _, _, offset, _) = decompose_tag::<T>(PPtr::<T>::null().into_offset());
        PShared {
            data: offset,
            _marker: PhantomData,
        }
    }

    /// Returns `true` if the pointer is null.
    ///
    /// # Examples
    ///
    /// ```
    /// # use memento::pmem::pool::*;
    /// # use memento::*;
    /// # use memento::test_utils::tests::get_dummy_handle;
    /// # let pool = get_dummy_handle(8 * 1024 * 1024 * 1024).unwrap();
    /// use memento::pepoch::{self as epoch, PAtomic, POwned};
    /// use std::sync::atomic::Ordering::SeqCst;
    ///
    /// // Assume there is PoolHandle, `pool`
    /// let a = PAtomic::null();
    /// let guard = &epoch::pin();
    /// assert!(a.load(SeqCst, guard).is_null());
    /// a.store(POwned::new(1234, &pool), SeqCst);
    /// assert!(!a.load(SeqCst, guard).is_null());
    /// ```
    #[allow(clippy::trivially_copy_pass_by_ref)]
    pub fn is_null(&self) -> bool {
        let (_, _, _, _, null_offset, _) = decompose_tag::<T>(PPtr::<T>::null().into_offset());
        let (_, _, _, _, my_offset, _) = decompose_tag::<T>(self.data);
        my_offset == null_offset
    }

    /// Dereferences the pointer.
    ///
    /// Returns a reference to the pointee that is valid during the lifetime `'g`.
    ///
    /// # Safety
    ///
    /// Dereferencing a pointer is unsafe because it could be pointing to invalid memory.
    ///
    /// Another concern is the possibility of data races due to lack of proper synchronization.
    /// For example, consider the following scenario:
    ///
    /// 1. A thread creates a new object: `a.store(POwned::new(10, &pool), Relaxed)`
    /// 2. Another thread reads it: `*a.load(Relaxed, guard).as_ref(&pool).unwrap()`
    ///
    /// The problem is that relaxed orderings don't synchronize initialization of the object with
    /// the read from the second thread. This is a data race. A possible solution would be to use
    /// `Release` and `Acquire` orderings.
    ///
    /// # Examples
    ///
    /// ```
    /// # use memento::pmem::pool::*;
    /// # use memento::*;
    /// # use memento::test_utils::tests::get_dummy_handle;
    /// # let pool = get_dummy_handle(8 * 1024 * 1024 * 1024).unwrap();
    /// use memento::pepoch::{self as epoch, PAtomic};
    /// use std::sync::atomic::Ordering::SeqCst;
    ///
    /// // Assume there is PoolHandle, `pool`
    /// let a = PAtomic::new(1234, &pool);
    /// let guard = &epoch::pin();
    /// let p = a.load(SeqCst, guard);
    /// unsafe {
    ///     assert_eq!(p.deref(&pool), &1234);
    /// }
    /// ```
    #[allow(clippy::trivially_copy_pass_by_ref)]
    #[allow(clippy::should_implement_trait)]
    pub unsafe fn deref(&self, pool: &'g PoolHandle) -> &'g T {
        let (_, _, _, _, offset, _) = decompose_tag::<T>(self.data);
        T::deref(offset, pool)
    }

    /// Dereferences the pointer.
    ///
    /// Returns a mutable reference to the pointee that is valid during the lifetime `'g`.
    ///
    /// # Safety
    ///
    /// * There is no guarantee that there are no more threads attempting to read/write from/to the
    ///   actual object at the same time.
    ///
    ///   The user must know that there are no concurrent accesses towards the object itself.
    ///
    /// * Other than the above, all safety concerns of `deref(&pool)` applies here.
    ///
    /// # Examples
    ///
    /// ```
    /// # use memento::pmem::pool::*;
    /// # use memento::*;
    /// # use memento::test_utils::tests::get_dummy_handle;
    /// # let pool = get_dummy_handle(8 * 1024 * 1024 * 1024).unwrap();
    /// use memento::pepoch::{self as epoch, PAtomic};
    /// use std::sync::atomic::Ordering::SeqCst;
    ///
    /// // Assume there is PoolHandle, `pool`
    /// let a = PAtomic::new(vec![1, 2, 3, 4], &pool);
    /// let guard = &epoch::pin();
    ///
    /// let mut p = a.load(SeqCst, guard);
    /// unsafe {
    ///     assert!(!p.is_null());
    ///     let b = p.deref_mut(&pool);
    ///     assert_eq!(b, &vec![1, 2, 3, 4]);
    ///     b.push(5);
    ///     assert_eq!(b, &vec![1, 2, 3, 4, 5]);
    /// }
    ///
    /// let p = a.load(SeqCst, guard);
    /// unsafe {
    ///     assert_eq!(p.deref(&pool), &vec![1, 2, 3, 4, 5]);
    /// }
    /// ```
    #[allow(clippy::should_implement_trait)]
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn deref_mut(&mut self, pool: &'g PoolHandle) -> &'g mut T {
        let (_, _, _, _, offset, _) = decompose_tag::<T>(self.data);
        T::deref_mut(offset, pool)
    }

    /// Converts the pointer to a reference.
    ///
    /// Returns `None` if the pointer is null, or else a reference to the object wrapped in `Some`.
    ///
    /// # Safety
    ///
    /// Dereferencing a pointer is unsafe because it could be pointing to invalid memory.
    ///
    /// Another concern is the possibility of data races due to lack of proper synchronization.
    /// For example, consider the following scenario:
    ///
    /// 1. A thread creates a new object: `a.store(Owned::new(10, &pool), Relaxed)`
    /// 2. Another thread reads it: `*a.load(Relaxed, guard).as_ref(&pool).unwrap()`
    ///
    /// The problem is that relaxed orderings don't synchronize initialization of the object with
    /// the read from the second thread. This is a data race. A possible solution would be to use
    /// `Release` and `Acquire` orderings.
    ///
    /// # Examples
    ///
    /// ```
    /// # use memento::pmem::pool::*;
    /// # use memento::*;
    /// # use memento::test_utils::tests::get_dummy_handle;
    /// # let pool = get_dummy_handle(8 * 1024 * 1024 * 1024).unwrap();
    /// use memento::pepoch::{self as epoch, PAtomic};
    /// use std::sync::atomic::Ordering::SeqCst;
    ///
    /// // Assume there is PoolHandle, `pool`
    /// let a = PAtomic::new(1234, &pool);
    /// let guard = &epoch::pin();
    /// let p = a.load(SeqCst, guard);
    /// unsafe {
    ///     assert_eq!(p.as_ref(&pool), Some(&1234));
    /// }
    /// ```
    #[allow(clippy::trivially_copy_pass_by_ref)]
    pub unsafe fn as_ref(&self, pool: &'g PoolHandle) -> Option<&'g T> {
        let (_, _, _, _, null_offset, _) = decompose_tag::<T>(PPtr::<T>::null().into_offset());
        let (_, _, _, _, my_offset, _) = decompose_tag::<T>(self.data);
        if my_offset == null_offset {
            None
        } else {
            Some(T::deref(my_offset, pool))
        }
    }

    /// Takes ownership of the pointee.
    ///
    /// # Panics
    ///
    /// Panics if this pointer is null, but only in debug mode.
    ///
    /// # Safety
    ///
    /// This method may be called only if the pointer is valid and nobody else is holding a
    /// reference to the same object.
    ///
    /// # Examples
    ///
    /// ```
    /// # use memento::pmem::pool::*;
    /// # use memento::*;
    /// # use memento::test_utils::tests::get_dummy_handle;
    /// # let pool = get_dummy_handle(8 * 1024 * 1024 * 1024).unwrap();
    /// use memento::pepoch::{self as epoch, PAtomic};
    /// use std::sync::atomic::Ordering::SeqCst;
    ///
    /// // Assume there is PoolHandle, `pool`
    /// let a = PAtomic::new(1234, &pool);
    /// unsafe {
    ///     let guard = &epoch::unprotected();
    ///     let p = a.load(SeqCst, guard);
    ///     drop(p.into_owned());
    /// }
    /// ```
    pub unsafe fn into_owned(self) -> POwned<T> {
        debug_assert!(!self.is_null(), "converting a null `PShared` into `POwned`");
        POwned::from_usize(self.data)
    }

    /// Returns the tag stored within the pointer.
    ///
    /// # Examples
    ///
    /// ```
    /// # use memento::pmem::pool::*;
    /// # use memento::*;
    /// # use memento::test_utils::tests::get_dummy_handle;
    /// # let pool = get_dummy_handle(8 * 1024 * 1024 * 1024).unwrap();
    /// use memento::pepoch::{self as epoch, PAtomic, POwned};
    /// use std::sync::atomic::Ordering::SeqCst;
    ///
    /// // Assume there is PoolHandle, `pool`
    /// let a = PAtomic::<u64>::from(POwned::new(0u64, &pool).with_tag(2));
    /// let guard = &epoch::pin();
    /// let p = a.load(SeqCst, guard);
    /// assert_eq!(p.tag(), 2);
    /// ```
    #[allow(clippy::trivially_copy_pass_by_ref)]
    pub fn tag(&self) -> usize {
        let (_, _, _, _, _, tag) = decompose_tag::<T>(self.data);
        tag
    }

    /// Get aux bit
    pub fn aux_bit(&self) -> usize {
        let (aux_bit, _, _, _, _, _) = decompose_tag::<T>(self.data);
        aux_bit
    }

    /// Get descripot bit
    pub fn desc_bit(&self) -> usize {
        let (_, desc_bit, _, _, _, _) = decompose_tag::<T>(self.data);
        desc_bit
    }

    /// Get tid
    pub fn tid(&self) -> usize {
        let (_, _, tid, _, _, _) = decompose_tag::<T>(self.data);
        tid
    }

    /// Get high tag
    pub fn high_tag(&self) -> usize {
        let (_, _, _, tag, _, _) = decompose_tag::<T>(self.data);
        tag
    }

    /// Returns the same pointer, but tagged with `tag`. `tag` is truncated to be fit into the
    /// unused bits of the pointer to `T`.
    ///
    /// # Examples
    ///
    /// ```
    /// # use memento::pmem::pool::*;
    /// # use memento::*;
    /// # use memento::test_utils::tests::get_dummy_handle;
    /// # let pool = get_dummy_handle(8 * 1024 * 1024 * 1024).unwrap();
    /// use memento::pepoch::{self as epoch, PAtomic};
    /// use std::sync::atomic::Ordering::SeqCst;
    ///
    /// // Assume there is PoolHandle, `pool`
    /// let a = PAtomic::new(0u64, &pool);
    /// let guard = &epoch::pin();
    /// let p1 = a.load(SeqCst, guard);
    /// let p2 = p1.with_tag(2);
    ///
    /// assert_eq!(p1.tag(), 0);
    /// assert_eq!(p2.tag(), 2);
    /// assert_eq!(p1.as_ptr(), p2.as_ptr());
    /// ```
    #[allow(clippy::trivially_copy_pass_by_ref)]
    pub fn with_tag(&self, tag: usize) -> PShared<'g, T> {
        unsafe { Self::from_usize(compose_tag::<T>(self.data, tag)) }
    }

    /// Set aux bit
    pub fn with_aux_bit(&self, aux_bit: usize) -> PShared<'g, T> {
        unsafe { Self::from_usize(compose_aux_bit(aux_bit, self.data)) }
    }

    /// Set descripot bit
    pub fn with_desc_bit(&self, desc_bit: usize) -> PShared<'g, T> {
        unsafe { Self::from_usize(compose_desc_bit(desc_bit, self.data)) }
    }

    /// Set tid
    pub fn with_tid(&self, tid: usize) -> PShared<'g, T> {
        unsafe { Self::from_usize(compose_tid(tid, self.data)) }
    }

    /// Returns the same pointer, but tagged with `tag`. `tag` is truncated to be fit into the
    /// unused high bits of the pointer to `T`.
    pub fn with_high_tag(&self, tag: usize) -> PShared<'g, T> {
        unsafe { Self::from_usize(compose_high_tag(tag, self.data)) }
    }

    /// formatting Pointer
    pub fn fmt(&self, f: &mut fmt::Formatter<'_>, pool: &PoolHandle) -> fmt::Result {
        fmt::Pointer::fmt(&(unsafe { self.deref(pool) as *const _ }), f)
    }
}

impl<T> From<PPtr<T>> for PShared<'_, T> {
    /// Returns a new pointer pointing to `ptr`.
    ///
    /// # Panics
    ///
    /// Panics if `ptr` is not properly aligned.
    ///
    /// # Examples
    ///
    /// ```
    /// # use memento::pmem::pool::*;
    /// # use memento::*;
    /// # use memento::test_utils::tests::get_dummy_handle;
    /// # let pool = get_dummy_handle(8 * 1024 * 1024 * 1024).unwrap();
    /// use memento::pepoch::PShared;
    ///
    /// // Assume there is PoolHandle, `pool`
    /// let ptr = pool.alloc::<usize>();
    /// let p = PShared::from(ptr);
    /// assert!(!p.is_null());
    /// ```
    fn from(ptr: PPtr<T>) -> Self {
        let offset = ptr.into_offset();
        ensure_aligned::<T>(offset);
        unsafe { Self::from_usize(offset) }
    }
}

impl<'g, T: ?Sized + Pointable> PartialEq<PShared<'g, T>> for PShared<'g, T> {
    fn eq(&self, other: &Self) -> bool {
        self.data == other.data
    }
}

impl<T: ?Sized + Pointable> Eq for PShared<'_, T> {}

impl<'g, T: ?Sized + Pointable> PartialOrd<PShared<'g, T>> for PShared<'g, T> {
    fn partial_cmp(&self, other: &Self) -> Option<cmp::Ordering> {
        self.data.partial_cmp(&other.data)
    }
}

impl<T: ?Sized + Pointable> Ord for PShared<'_, T> {
    fn cmp(&self, other: &Self) -> cmp::Ordering {
        self.data.cmp(&other.data)
    }
}

impl<T: ?Sized + Pointable> fmt::Debug for PShared<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (aux_bit, desc_bit, tid, high_tag, offset, tag) = decompose_tag::<T>(self.data);

        f.debug_struct("Shared")
            .field("aux bit", &aux_bit)
            .field("desc bit", &desc_bit)
            .field("tid", &tid)
            .field("high tag", &high_tag)
            .field("offset", &offset)
            .field("tag", &tag)
            .finish()
    }
}

impl<T: ?Sized + Pointable> Default for PShared<'_, T> {
    fn default() -> Self {
        PShared::null()
    }
}

#[cfg(all(test, not(crossbeam_loom)))]
mod tests {
    use super::{POwned, PShared};
    use rusty_fork::rusty_fork_test;
    use std::mem::MaybeUninit;

    use crate::test_utils::tests::*;

    #[test]
    fn valid_tag_i8() {
        let _ = PShared::<i8>::null().with_tag(0);
    }

    #[test]
    fn valid_tag_i64() {
        let _ = PShared::<i64>::null().with_tag(7);
    }

    #[cfg(feature = "nightly")]
    #[test]
    fn const_atomic_null() {
        use super::PAtomic;
        static _U: PAtomic<u8> = PAtomic::<u8>::null();
    }

    rusty_fork_test! {
        #[test]
        fn array_init() {
            let pool = get_dummy_handle(8 * 1024 * 1024 * 1024).unwrap();
            let owned = POwned::<[MaybeUninit<usize>]>::init(10, pool);
            let arr: &[MaybeUninit<usize>] = unsafe { owned.deref(&pool) };
            assert_eq!(arr.len(), 10);
        }
    }
}
