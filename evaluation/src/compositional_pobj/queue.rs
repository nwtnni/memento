use core::sync::atomic::Ordering;
use crossbeam_epoch::{self as epoch, Guard};
use crossbeam_utils::CachePadded;
use memento::persistent::*;
use memento::plocation::pool::*;
use memento::plocation::ralloc::{Collectable, GarbageCollection};
use memento::queue::*;

use crate::common::queue::{enq_deq_pair, enq_deq_prob, TestQueue};
use crate::common::{TestNOps, DURATION, PROB, QUEUE_INIT_SIZE, TOTAL_NOPS};

// for<'o> Memento<Object<'o> = &'o O, Input = usize>

impl<T: 'static + Clone> TestQueue for Queue<T> {
    type EnqInput = (&'static mut Enqueue<T>, T); // Memento, input
    type DeqInput = &'static mut Dequeue<T>; // Memento

    fn enqueue(&self, (enq, input): Self::EnqInput, guard: &mut Guard, pool: &'static PoolHandle) {
        let _ = enq.run(self, input, guard, pool);
        enq.reset(false, guard, pool);
    }

    fn dequeue(&self, deq: Self::DeqInput, guard: &mut Guard, pool: &'static PoolHandle) {
        let _ = deq.run(self, (), guard, pool);
        deq.reset(false, guard, pool);
    }
}

/// 초기화시 세팅한 노드 수만큼 넣어줌  
#[derive(Debug)]
pub struct TestMementoQueue {
    queue: Queue<usize>,
}

impl Collectable for TestMementoQueue {
    fn filter(_: &mut Self, _: &mut GarbageCollection, _: &PoolHandle) {
        todo!()
    }
}

impl PDefault for TestMementoQueue {
    fn pdefault(pool: &'static PoolHandle) -> Self {
        let queue = Queue::pdefault(pool);
        let mut guard = epoch::pin();

        // 초기 노드 삽입
        let mut push_init = Enqueue::default();
        for i in 0..QUEUE_INIT_SIZE {
            let _ = push_init.run(&queue, i, &mut guard, pool);
            push_init.reset(false, &mut guard, pool);
        }
        Self { queue }
    }
}

#[derive(Debug)]
pub struct MementoQueueEnqDeqPair {
    push: CachePadded<Enqueue<usize>>,
    pop: CachePadded<Dequeue<usize>>,
}

impl Default for MementoQueueEnqDeqPair {
    fn default() -> Self {
        Self {
            push: CachePadded::new(Enqueue::<usize>::default()),
            pop: CachePadded::new(Dequeue::<usize>::default()),
        }
    }
}

impl Collectable for MementoQueueEnqDeqPair {
    fn filter(_: &mut Self, _: &mut GarbageCollection, _: &PoolHandle) {
        todo!()
    }
}

impl TestNOps for MementoQueueEnqDeqPair {}

impl Memento for MementoQueueEnqDeqPair {
    type Object<'o> = &'o TestMementoQueue;
    type Input = usize; // tid
    type Output<'o> = ();
    type Error = ();

    fn run<'o>(
        &'o mut self,
        queue: Self::Object<'o>,
        tid: Self::Input,
        guard: &mut Guard,
        pool: &'static PoolHandle,
    ) -> Result<Self::Output<'o>, Self::Error> {
        let q = &queue.queue;
        let duration = unsafe { DURATION };

        let ops = self.test_nops(
            &|tid, guard| {
                let push =
                    unsafe { (&*self.push as *const _ as *mut Enqueue<usize>).as_mut() }.unwrap();
                let pop =
                    unsafe { (&*self.pop as *const _ as *mut Dequeue<usize>).as_mut() }.unwrap();
                let enq_input = (push, tid);
                let deq_input = pop;
                enq_deq_pair(q, enq_input, deq_input, guard, pool);
            },
            tid,
            duration,
            guard,
        );

        let _ = TOTAL_NOPS.fetch_add(ops, Ordering::SeqCst);

        Ok(())
    }

    fn reset(&mut self, _: bool, _: &mut Guard, _: &'static PoolHandle) {
        // no-op
    }

    fn set_recovery(&mut self, _: &'static PoolHandle) {
        // no-op
    }
}

#[derive(Debug)]
pub struct MementoQueueEnqDeqProb {
    push: CachePadded<Enqueue<usize>>,
    pop: CachePadded<Dequeue<usize>>,
}

impl Default for MementoQueueEnqDeqProb {
    fn default() -> Self {
        Self {
            push: CachePadded::new(Enqueue::<usize>::default()),
            pop: CachePadded::new(Dequeue::<usize>::default()),
        }
    }
}

impl Collectable for MementoQueueEnqDeqProb {
    fn filter(_: &mut Self, _: &mut GarbageCollection, _: &PoolHandle) {
        todo!()
    }
}

impl TestNOps for MementoQueueEnqDeqProb {}

impl Memento for MementoQueueEnqDeqProb {
    type Object<'o> = &'o TestMementoQueue;
    type Input = usize; // tid
    type Output<'o> = ();
    type Error = ();

    fn run<'o>(
        &'o mut self,
        queue: Self::Object<'o>,
        tid: Self::Input,
        guard: &mut Guard,
        pool: &'static PoolHandle,
    ) -> Result<Self::Output<'o>, Self::Error> {
        let q = &queue.queue;
        let duration = unsafe { DURATION };
        let prob = unsafe { PROB };

        let ops = self.test_nops(
            &|tid, guard| {
                let push =
                    unsafe { (&*self.push as *const _ as *mut Enqueue<usize>).as_mut() }.unwrap();
                let pop =
                    unsafe { (&*self.pop as *const _ as *mut Dequeue<usize>).as_mut() }.unwrap();
                let enq_input = (push, tid);
                let deq_input = pop;
                enq_deq_prob(q, enq_input, deq_input, prob, guard, pool);
            },
            tid,
            duration,
            guard,
        );

        let _ = TOTAL_NOPS.fetch_add(ops, Ordering::SeqCst);

        Ok(())
    }

    fn reset(&mut self, _: bool, _: &mut Guard, _: &'static PoolHandle) {
        // no-op
    }

    fn set_recovery(&mut self, _: &'static PoolHandle) {
        // no-op
    }
}