//! Detectable Combining Operation
#![allow(missing_docs)]
use crate::pepoch::PAtomic;
use crate::ploc::Handle;
use crate::pmem::{persist_obj, Collectable, GarbageCollection, PoolHandle};
use crate::Memento;
use array_init::array_init;
use crossbeam_epoch::Guard;
use crossbeam_utils::{Backoff, CachePadded};
use libc::c_void;
use mmt_derive::Collectable;
use std::sync::atomic::{AtomicUsize, Ordering};

use self::combining_lock::CombiningLock;

pub const MAX_THREADS: usize = 64;
const COMBINING_ROUNDS: usize = 20;

/// restriction of combining iteration
pub static mut NR_THREADS: usize = MAX_THREADS;

/// Node
#[derive(Debug, Collectable)]
#[repr(align(128))]
pub struct Node {
    pub data: usize,
    pub next: PAtomic<Node>,
}

/// Trait for Memento
pub trait Combinable: Memento {
    // checkpoint activate of request
    fn chk_activate(&mut self, activate: usize, handle: &Handle) -> usize;

    fn peek_retval(&mut self) -> usize;

    fn backup_retval(&mut self, return_value: usize);
}

/// request obj
#[derive(Default, Debug, Collectable)]
pub struct CombRequest {
    arg: AtomicUsize,
    activate: AtomicUsize,
}

/// state obj
#[derive(Debug)]
pub struct CombStateRec {
    pub data: PAtomic<c_void>, // The actual data of the state (e.g. tail for enqueue, head for dequeue)
    return_value: [usize; MAX_THREADS + 1],
    deactivate: [AtomicUsize; MAX_THREADS + 1],
}

impl CombStateRec {
    pub fn new<T>(data: PAtomic<T>) -> Self {
        Self {
            data: unsafe { (&data as *const _ as *const PAtomic<c_void>).read() },
            return_value: array_init(|_| Default::default()),
            deactivate: array_init(|_| Default::default()),
        }
    }
}

impl Collectable for CombStateRec {
    fn filter(s: &mut Self, tid: usize, gc: &mut GarbageCollection, pool: &mut PoolHandle) {
        Collectable::filter(&mut s.data, tid, gc, pool);
    }
}

impl Clone for CombStateRec {
    fn clone(&self) -> Self {
        Self {
            data: self.data.clone(),
            return_value: array_init(|i| self.return_value[i]),
            deactivate: array_init(|i| {
                AtomicUsize::new(self.deactivate[i].load(Ordering::Relaxed))
            }),
        }
    }
}

/// per-thread state for combining
#[derive(Debug)]
pub struct CombThreadState {
    index: AtomicUsize,
    state: [PAtomic<CombStateRec>; 2],
}

impl CombThreadState {
    pub fn new<T>(data: PAtomic<T>, pool: &PoolHandle) -> Self {
        Self {
            index: Default::default(),
            state: array_init(|_| PAtomic::new(CombStateRec::new(data.clone()), pool)),
        }
    }
}

impl Collectable for CombThreadState {
    fn filter(s: &mut Self, tid: usize, gc: &mut GarbageCollection, pool: &mut PoolHandle) {
        Collectable::filter(&mut s.state[0], tid, gc, pool);
        Collectable::filter(&mut s.state[1], tid, gc, pool);
    }
}

/// Central object for combining
#[allow(missing_debug_implementations)]
pub struct CombStruct {
    // General func for additional behavior: e.g. persist enqueued nodes
    final_func: Option<&'static dyn Fn(&CombStruct, &Guard, &PoolHandle)>,
    after_func: Option<&'static dyn Fn(&CombStruct, &Guard, &PoolHandle)>,

    // Variables located at volatile location
    lock: &'static CachePadded<CombiningLock>,

    // Variables located at persistent location
    request: [CachePadded<CombRequest>; MAX_THREADS + 1], // per-thread requests
    pub pstate: CachePadded<PAtomic<CombStateRec>>,       // stable state
}

impl Collectable for CombStruct {
    fn filter(s: &mut Self, tid: usize, gc: &mut GarbageCollection, pool: &mut PoolHandle) {
        for t in 0..s.request.len() {
            Collectable::filter(&mut *s.request[t], tid, gc, pool);
        }
        Collectable::filter(&mut *s.pstate, tid, gc, pool);
    }
}

impl CombStruct {
    pub fn new(
        final_func: Option<&'static dyn Fn(&CombStruct, &Guard, &PoolHandle)>,
        after_func: Option<&'static dyn Fn(&CombStruct, &Guard, &PoolHandle)>,
        lock: &'static CachePadded<CombiningLock>,
        request: [CachePadded<CombRequest>; MAX_THREADS + 1],
        pstate: CachePadded<PAtomic<CombStateRec>>,
    ) -> Self {
        Self {
            final_func,
            after_func,
            lock,
            request,
            pstate,
        }
    }
}

#[derive(Debug)]
pub struct Combining {}

impl Combining {
    // sfunc: (state data (head or tail), arg, tid, guard, pool) -> return value
    pub fn apply_op<M: Combinable>(
        arg: usize,
        (s, st_thread, sfunc): (
            &CombStruct,
            &CombThreadState,
            &dyn Fn(&PAtomic<c_void>, usize, &Handle) -> usize,
        ),
        mmt: &mut M,
        handle: &Handle,
    ) -> usize {
        let (tid, guard, pool) = (handle.tid, &handle.guard, handle.pool);

        // Checkpoint activate
        let activate =
            mmt.chk_activate(s.request[tid].activate.load(Ordering::Relaxed) + 1, handle);

        // Check if my request was already performed.
        if handle.rec.load(Ordering::Relaxed) {
            let latest_state = unsafe { s.pstate.load(Ordering::SeqCst, guard).deref(pool) };
            let deactivate = latest_state.deactivate[tid].load(Ordering::SeqCst);
            if activate < deactivate {
                return mmt.peek_retval();
            }

            // if i was a combiner, i have to finalize the combine.
            if activate == deactivate && !s.lock.is_owner(tid) {
                let retval = latest_state.return_value[tid];
                mmt.backup_retval(retval);
                return retval;
            }

            if activate < s.request[tid].activate.load(Ordering::Relaxed) {
                return mmt.peek_retval();
            }
            handle.rec.store(false, Ordering::Relaxed);
        }

        // Register request
        s.request[tid].arg.store(arg, Ordering::Relaxed);
        s.request[tid].activate.store(activate, Ordering::Release);

        // Do
        loop {
            match s.lock.try_lock(tid) {
                Ok(_) => return Self::do_combine((s, st_thread, sfunc), mmt, handle),
                Err(_) => {
                    if let Ok(retval) = Self::do_non_combine(s, mmt, handle.tid) {
                        return retval;
                    }
                }
            }
        }
    }

    /// combiner performs the requests
    ///
    /// 1. ready: copy central state to my thread-local state, pt.state[pt.index]
    /// 2. perform: update pt.state[pt.index]
    /// 3. finalize:
    ///     3.1. central state = pt.state[pt.index] (commit point)
    ///     3.2. pt.index = 1 - pt.index
    ///     3.3. release lock
    fn do_combine<M: Combinable>(
        (s, st_thread, sfunc): (
            &CombStruct,
            &CombThreadState,
            &dyn Fn(
                // This is data of CombStruct. (e.g. `tail` for Enqueue combiner, `head` for Dequeue combiner)
                // Combiner will give stable value for this argument using old/new flipping logic.
                &PAtomic<c_void>,

                // Arugment
                // Combiner will give stable value for this argument using old/new flipping logic.
                usize,
                &Handle,
            ) -> usize,
        ),
        mmt: &mut M,
        handle: &Handle,
    ) -> usize {
        let (tid, guard, pool) = (handle.tid, &handle.guard, handle.pool);

        // ready
        let ind = st_thread.index.load(Ordering::Relaxed);
        let mut new_state = st_thread.state[ind].load(Ordering::Relaxed, guard);
        let new_state_ref = unsafe { new_state.deref_mut(pool) };
        *new_state_ref = unsafe { s.pstate.load(Ordering::Relaxed, guard).deref(pool) }.clone(); // create a copy of current state

        // perform requests
        for _ in 0..COMBINING_ROUNDS {
            let mut serve_reqs = 0;

            for t in 1..unsafe { NR_THREADS } + 1 {
                let t_activate = s.request[t].activate.load(Ordering::Acquire);
                if t_activate > new_state_ref.deactivate[t].load(Ordering::Relaxed) {
                    new_state_ref.return_value[t] = sfunc(
                        &new_state_ref.data,
                        s.request[t].arg.load(Ordering::Relaxed),
                        handle,
                    );
                    new_state_ref.deactivate[t].store(t_activate, Ordering::Release);

                    // cnt
                    serve_reqs += 1;
                }
            }

            if serve_reqs == 0 {
                break;
            }
        }

        // e.g. enqueue: persist all enqueued node
        if let Some(func) = s.final_func {
            func(s, guard, pool);
        }
        persist_obj(new_state_ref, true);

        // 3.1 central state = pt.state[pt.index] (commit point)
        s.pstate.store(new_state, Ordering::Release);
        persist_obj(&*s.pstate, true);

        // e.g. enqueue: update old tail
        if let Some(func) = s.after_func {
            func(s, guard, pool);
        }

        // 3.2. flip per-thread index
        st_thread.index.store(1 - ind, Ordering::Relaxed);

        // 3.3. release lock with new state
        unsafe { s.lock.unlock(new_state_ref as *const _ as usize) };

        let retval = new_state_ref.return_value[tid];
        mmt.backup_retval(retval);
        retval
    }

    /// non-combiner (1) wait until combiner unlocks the lock and (2) check if my request was performed (3) return
    fn do_non_combine<M: Combinable>(
        // &self,
        s: &CombStruct,
        mmt: &mut M,
        tid: usize,
    ) -> Result<usize, ()> {
        // wait until the combiner unlocks the lock
        let backoff = Backoff::new();
        let mut combined_ptr;
        let mut combined_tid;
        loop {
            (combined_ptr, combined_tid) = s.lock.peek();
            if combined_tid == 0 {
                break;
            }
            #[cfg(not(feature = "pmcheck"))]
            backoff.snooze();
            #[cfg(feature = "pmcheck")]
            println!("[do_non_combine] ptr: {combined_ptr}, tid: {combined_tid}");
        }

        // check if my request was performed
        let lastest_state = unsafe { (combined_ptr as *const CombStateRec).as_ref().unwrap() };
        if s.request[tid].activate.load(Ordering::Relaxed)
            <= lastest_state.deactivate[tid].load(Ordering::Acquire)
        {
            let retval = lastest_state.return_value[tid];
            mmt.backup_retval(retval);
            return Ok(retval);
        }

        Err(())
    }
}

pub mod combining_lock {
    //! Thread-recoverable lock for combining
    use core::sync::atomic::Ordering;
    use std::sync::atomic::AtomicUsize;

    use crate::impl_left_bits;

    // Auxiliary Bits
    // aux bits: MSB 55-bit in 64-bit
    // Used for:
    // - Comb: Indicating ptr of combined state
    pub(crate) const POS_AUX_BITS: u32 = 0;
    pub(crate) const NR_AUX_BITS: u32 = 55;
    impl_left_bits!(aux_bits, POS_AUX_BITS, NR_AUX_BITS, usize);

    #[inline]
    fn compose_aux_bit(aux: usize, data: usize) -> usize {
        (aux_bits() & (aux.rotate_right(POS_AUX_BITS + NR_AUX_BITS))) | (!aux_bits() & data)
    }

    #[inline]
    fn decompose_aux_bit(data: usize) -> (usize, usize) {
        (
            (data & aux_bits()).rotate_left(POS_AUX_BITS + NR_AUX_BITS),
            !aux_bits() & data,
        )
    }

    /// thread-recoverable spin lock
    #[derive(Debug, Default)]
    pub struct CombiningLock {
        inner: AtomicUsize, // 55:ptr of state, 9:tid occupying the lock
    }

    impl CombiningLock {
        const PTR_NULL: usize = 0;
        const RELEASED: usize = 0;

        /// Try lock
        ///
        /// return Ok: (seq, guard)
        /// return Err: (seq, tid)
        pub fn try_lock(&self, tid: usize) -> Result<(), (usize, usize)> {
            let current = self.inner.load(Ordering::Relaxed);
            let (_ptr, _tid) = decompose_aux_bit(current);

            if self.is_owner(tid) {
                return Ok(());
            }

            if _tid != Self::RELEASED {
                return Err((_ptr, _tid));
            }

            self.inner
                .compare_exchange(
                    current,
                    compose_aux_bit(Self::PTR_NULL, tid),
                    Ordering::Acquire,
                    Ordering::Relaxed,
                )
                .map(|_| ())
                .map_err(|_| (_ptr, _tid))
        }

        /// peek
        ///
        /// return (ptr, tid)
        pub fn peek(&self) -> (usize, usize) {
            decompose_aux_bit(self.inner.load(Ordering::Acquire))
        }

        pub fn is_owner(&self, tid: usize) -> bool {
            let current = self.inner.load(Ordering::Relaxed);
            let (_, _tid) = decompose_aux_bit(current);

            tid == _tid
        }

        /// unlock
        ///
        /// # Safety
        ///
        /// Only the thread who get `Ok()` as return value from `try_lock()` should call `unlock()`. Also, that thread shouldn't call twice or more for one `try_lock()`.
        pub unsafe fn unlock(&self, ptr: usize) {
            self.inner
                .store(compose_aux_bit(ptr, Self::RELEASED), Ordering::Release);
        }
    }
}
