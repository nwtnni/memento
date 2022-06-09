use std::sync::atomic::{AtomicUsize, Ordering};

use crossbeam_epoch::{unprotected, Guard};
use etrace::ok_or;
use evaluation::common::{DURATION, TOTAL_NOPS};
use memento::{
    pepoch::{
        atomic::{CompareExchangeError, Pointer},
        PAtomic, PShared,
    },
    pmem::{persist_obj, AsPPtr, Collectable, GarbageCollection, PPtr, PoolHandle, RootObj},
    PDefault,
};

use crate::{Node, TestNOps};

pub struct TestPCas {
    loc: PAtomic<Node>,
}

impl Collectable for TestPCas {
    fn filter(_: &mut Self, _: usize, _: &mut GarbageCollection, _: &mut PoolHandle) {
        todo!()
    }
}

impl PDefault for TestPCas {
    fn pdefault(_: &PoolHandle) -> Self {
        Self {
            loc: Default::default(),
        }
    }
}

impl TestNOps for TestPCas {}

#[derive(Default, Debug)]
pub struct TestPCasMmt {}

impl Collectable for TestPCasMmt {
    fn filter(_: &mut Self, _: usize, _: &mut GarbageCollection, _: &mut PoolHandle) {
        todo!()
    }
}

impl RootObj<TestPCasMmt> for TestPCas {
    fn run(&self, _: &mut TestPCasMmt, tid: usize, _: &Guard, _: &PoolHandle) {
        let duration = unsafe { DURATION };

        let ops = self.test_nops(&|tid| pcas(&self.loc, tid), tid, duration);

        let _ = TOTAL_NOPS.fetch_add(ops, Ordering::SeqCst);
    }
}

fn pcas(loc: &PAtomic<Node>, tid: usize) -> bool {
    let guard = unsafe { unprotected() };

    let old = loc.load(Ordering::SeqCst, guard);
    let new = unsafe { PShared::from_usize(tid) }; // TODO: 다양한 new 값
    persistent_cas(loc, old, new, guard).is_ok()
}

const DIRTY_FLAG: usize = 1;
const PMWCAS_FLAG: usize = 2;
const RDCSS_FLAG: usize = 4;

fn pcas_read<'g>(address: &PAtomic<Node>, guard: &'g Guard) -> PShared<'g, Node> {
    let word = address.load(Ordering::SeqCst, guard);
    if word.tag() & DIRTY_FLAG != 0 {
        persist(address, word, guard);
    }
    word.with_tag(0)
}

fn persistent_cas<'g>(
    address: &PAtomic<Node>,
    old_value: PShared<Node>,
    new_value: PShared<'g, Node>,
    guard: &'g Guard,
) -> Result<PShared<'g, Node>, CompareExchangeError<'g, Node, PShared<'g, Node>>> {
    let _ = pcas_read(address, guard);
    // Conduct the CAS with dirty bit set on new value
    address.compare_exchange(
        old_value,
        new_value.with_tag(DIRTY_FLAG),
        Ordering::SeqCst,
        Ordering::SeqCst,
        guard,
    )
}

fn persist<T>(address: &PAtomic<T>, value: PShared<T>, guard: &Guard) {
    persist_obj(address, true);
    let _ = address.compare_exchange(
        value,
        value.with_tag(0),
        Ordering::SeqCst,
        Ordering::SeqCst,
        guard,
    );
}

const UNDECIDED: usize = 0;
const SUCCEEDED: usize = 2;
const FAILED: usize = 4;

#[derive(Default, Clone)]
struct WordDescriptor {
    address: PPtr<PAtomic<PMwCasDescriptor>>,
    old_value: PPtr<Node>,
    new_value: PPtr<Node>,
    mwcas_descriptor: PPtr<PMwCasDescriptor>,
}

struct PMwCasDescriptor {
    status: AtomicUsize,
    count: usize,
    words: [WordDescriptor; 4],
}

impl Default for PMwCasDescriptor {
    fn default() -> Self {
        Self {
            status: AtomicUsize::new(UNDECIDED),
            count: 0,
            words: [
                Default::default(),
                Default::default(),
                Default::default(),
                Default::default(),
            ],
        }
    }
}

pub struct TestPMwCas {
    loc: PAtomic<Node>,
}

impl Collectable for TestPMwCas {
    fn filter(_: &mut Self, _: usize, _: &mut GarbageCollection, _: &mut PoolHandle) {
        todo!()
    }
}

impl PDefault for TestPMwCas {
    fn pdefault(_: &PoolHandle) -> Self {
        Self {
            loc: Default::default(),
        }
    }
}

impl TestNOps for TestPMwCas {}

#[derive(Default, Debug)]
pub struct TestPMwCasMmt {}

impl Collectable for TestPMwCasMmt {
    fn filter(_: &mut Self, _: usize, _: &mut GarbageCollection, _: &mut PoolHandle) {
        todo!()
    }
}

impl RootObj<TestPMwCasMmt> for TestPMwCas {
    fn run(&self, _: &mut TestPMwCasMmt, tid: usize, _: &Guard, _: &PoolHandle) {
        let duration = unsafe { DURATION };

        let ops = self.test_nops(&|tid| pcas(&self.loc, tid), tid, duration);

        let _ = TOTAL_NOPS.fetch_add(ops, Ordering::SeqCst);
    }
}

fn pmwcas(loc: &PAtomic<Node>, tid: usize, pool: &PoolHandle) -> bool {
    let guard = unsafe { unprotected() };

    let old = loc.load(Ordering::SeqCst, guard);
    let new = unsafe { PShared::<Node>::from_usize(tid) }; // TODO: 다양한 new 값

    let mut desc = PMwCasDescriptor::default(); // TODO: ploc에서 만들어져야 함
    unsafe {
        desc.words[0] = WordDescriptor {
            address: PPtr::from(loc.as_pptr(pool).into_offset()),
            old_value: old.deref(pool).as_pptr(pool),
            new_value: new.deref(pool).as_pptr(pool),
            mwcas_descriptor: desc.as_pptr(pool),
        };
    }

    persistent_cas(loc, old, new, guard).is_ok()
}

// TODO: 실험을 위해선 (1) Descriptor에 location 쓰고 (2) Descriptor를 pmwcas에 넘길 것
fn pmwcas_inner(md: &PMwCasDescriptor, guard: &Guard, pool: &PoolHandle) -> bool {
    let md_ptr = unsafe { PShared::from(md.as_pptr(pool)) };
    let mut st = SUCCEEDED;

    'out: for w in md.words.iter() {
        // (in sorted order on md.words.address)
        loop {
            let rval = install_mwcas_descriptor(w, guard, pool);
            let rval_high = rval.high_tag();
            let old = w.old_value;

            if rval.into_usize() == old.into_offset() {
                // Descriptor successfully installed
                continue 'out;
            } else if rval_high & PMWCAS_FLAG != 0 {
                if rval_high & DIRTY_FLAG != 0 {
                    persist(unsafe { w.address.deref(pool) }, rval, guard);
                }
                // Clashed another on-going MwCAS, help it finish
                let rval_addr = unsafe { rval.deref(pool) };
                pmwcas_inner(rval_addr, guard, pool);
                continue;
            } else {
                st = FAILED;
                break 'out;
            }
        }
    }

    // Persist all target words if Phase 1 succeeded
    if st == SUCCEEDED {
        for w in md.words.iter() {
            persist(
                unsafe { w.address.deref(pool) },
                md_ptr.with_high_tag(PMWCAS_FLAG | DIRTY_FLAG),
                guard,
            );
        }
    }

    // Finalize the MwCAS’s status
    let mut cur_st = ok_or!(
        md.status.compare_exchange(
            UNDECIDED,
            st | DIRTY_FLAG,
            Ordering::SeqCst,
            Ordering::SeqCst,
        ),
        e,
        e
    );
    if cur_st & DIRTY_FLAG != 0 {
        persist_obj(&md.status, true);
        cur_st &= !DIRTY_FLAG;
        md.status.store(cur_st, Ordering::SeqCst);
    }

    // Install the final values
    for w in md.words.iter() {
        let v = if cur_st == SUCCEEDED {
            w.new_value
        } else {
            w.old_value
        };
        let v = unsafe { PShared::from_usize(v.into_offset()) };
        let expected = md_ptr.with_high_tag(PMWCAS_FLAG | DIRTY_FLAG);
        let target = unsafe { w.address.deref(pool) };
        let rval = if let Err(e) = target.compare_exchange(
            expected,
            v.with_high_tag(DIRTY_FLAG),
            Ordering::SeqCst,
            Ordering::SeqCst,
            guard,
        ) {
            e.current
        } else {
            expected
        };
        if rval == md_ptr.with_high_tag(PMWCAS_FLAG) {
            let _ = target.compare_exchange(rval, v, Ordering::SeqCst, Ordering::SeqCst, guard);
        }
        persist(target, v, guard);
    }

    cur_st == SUCCEEDED
}

fn install_mwcas_descriptor<'g>(
    wd: &WordDescriptor,
    guard: &'g Guard,
    pool: &PoolHandle,
) -> PShared<'g, PMwCasDescriptor> {
    let ptr =
        unsafe { PShared::from_usize(wd.as_pptr(pool).into_offset()) }.with_high_tag(RDCSS_FLAG);
    let val = loop {
        let target = unsafe { wd.address.deref(pool) };
        let expected = unsafe { PShared::from_usize(wd.old_value.into_offset()) };
        let val = if let Err(e) = target.compare_exchange(
            unsafe { PShared::from_usize(wd.old_value.into_offset()) },
            ptr,
            Ordering::SeqCst,
            Ordering::SeqCst,
            guard,
        ) {
            e.current
        } else {
            expected
        };

        if val.high_tag() & RDCSS_FLAG == 0 {
            break val;
        }

        // Hit another RDCSS operation, help it finish
        let cur = unsafe { PShared::from_usize(val.into_usize()).deref(pool) };
        complete_install(cur, guard, pool);
    };

    if val.into_usize() == wd.old_value.into_offset() {
        // # Successfully installed the RDCSS descriptor
        complete_install(wd, guard, pool);
    }

    val
}

fn complete_install(wd: &WordDescriptor, guard: &Guard, pool: &PoolHandle) {
    let desc = PShared::from(wd.mwcas_descriptor);
    let ptr = desc.with_high_tag(PMWCAS_FLAG | DIRTY_FLAG);
    let u = unsafe { desc.deref(pool) }.status.load(Ordering::SeqCst) == UNDECIDED;

    let target = unsafe { wd.address.deref(pool) };
    let old = unsafe { PShared::from_usize(wd as *const _ as _) }.with_high_tag(RDCSS_FLAG);
    let new = if u {
        ptr
    } else {
        unsafe { PShared::from_usize(wd.old_value.into_offset()) }
    };
    let _ = target.compare_exchange(old, new, Ordering::SeqCst, Ordering::SeqCst, guard);
}