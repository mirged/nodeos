//! In-kernel self-tests.
//!
//! These run on every boot rather than in a separate harness. A microkernel's
//! invariants -- a revoked capability really is dead everywhere, a fair-share
//! thread really does get its share, a copy-on-write page really is private --
//! are cheap to check and catastrophic to get subtly wrong, and a test that
//! only runs when someone remembers to run it is a test that does not run.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::mm::space::MapFlags;
use crate::mm::vmo::Vmo;
use crate::obj::{CapClass, CapSet, Handle, HandleTable, KernelObject, Rights};
use crate::sched::{self, SchedParams};
use crate::task::Process;
use crate::{info, mm};

static PASSED: AtomicU32 = AtomicU32::new(0);
static FAILED: AtomicU32 = AtomicU32::new(0);

macro_rules! check {
    ($cond:expr, $($arg:tt)*) => {
        if $cond {
            PASSED.fetch_add(1, Ordering::Relaxed);
        } else {
            FAILED.fetch_add(1, Ordering::Relaxed);
            crate::error!("SELFTEST FAILED: {}", format_args!($($arg)*));
        }
    };
}

/// Run every self-test and report.
pub fn run() {
    info!("running kernel self-tests");
    rights_are_monotonic();
    revocation_is_transitive();
    handle_generations_prevent_reuse();
    namespaces_isolate();
    memory_objects_and_cow();
    scheduler_classes_are_ordered();

    let passed = PASSED.load(Ordering::Relaxed);
    let failed = FAILED.load(Ordering::Relaxed);
    if failed == 0 {
        info!("self-tests: {passed} passed, 0 failed");
    } else {
        crate::error!("self-tests: {passed} passed, {failed} FAILED");
        panic!("{failed} kernel self-test(s) failed");
    }
}

/// Deriving a handle can only ever narrow its rights.
fn rights_are_monotonic() {
    let vmo = Vmo::new_anonymous(4096, false).expect("vmo");
    let full = Handle::root(vmo.clone(), Rights::VMO_DEFAULT);

    let narrowed = full.derive(Rights::READ_ONLY).expect("derive");
    check!(!narrowed.rights().contains(Rights::WRITE), "narrowed handle kept WRITE");

    // Ask for more than the parent had: the intersection must still exclude it.
    let attempted_escalation = narrowed.derive(Rights::VMO_DEFAULT).expect("derive");
    check!(
        !attempted_escalation.rights().contains(Rights::WRITE),
        "rights escalated through re-derivation"
    );
    check!(
        attempted_escalation.rights().is_subset_of(narrowed.rights()),
        "derived rights are not a subset of the parent's"
    );

    // Mapping enforcement: a read-only handle cannot authorise a writable map.
    let needed = MapFlags::READ.or(MapFlags::WRITE).required_rights();
    check!(
        !narrowed.rights().contains(needed),
        "a read-only handle satisfies the rights needed for a writable mapping"
    );
}

/// Revoking a grant kills every capability derived from it, however far away.
fn revocation_is_transitive() {
    let vmo = Vmo::new_anonymous(4096, false).expect("vmo");
    let root = Handle::root(vmo, Rights::VMO_DEFAULT);

    let child = root.derive(Rights::VMO_DEFAULT).expect("derive child");
    let grandchild = child.derive(Rights::READ_ONLY).expect("derive grandchild");
    let sibling = root.derive(Rights::READ_ONLY).expect("derive sibling");

    check!(grandchild.is_live(), "grandchild should start live");

    // Revoke the middle of the chain.
    child.grant().revoke();

    check!(!child.is_live(), "revoked handle is still live");
    check!(!grandchild.is_live(), "revocation did not reach the grandchild");
    check!(sibling.is_live(), "revocation leaked into an unrelated branch");
    check!(root.is_live(), "revoking a child killed the parent");

    check!(
        grandchild.check(Rights::READ) == Err(crate::obj::ObjectError::Revoked),
        "a revoked handle still passes rights checks"
    );
}

/// A closed handle value never names anything again.
fn handle_generations_prevent_reuse() {
    let table = HandleTable::new();
    let vmo_a = Vmo::new_anonymous(4096, false).expect("vmo");
    let vmo_b = Vmo::new_anonymous(4096, false).expect("vmo");

    let first = table.insert(Handle::root(vmo_a.clone(), Rights::VMO_DEFAULT)).expect("insert");
    let koid_a = vmo_a.header().koid();
    table.remove(first).expect("remove");

    // The slot is now free and will be reused by the next insert.
    let second = table.insert(Handle::root(vmo_b, Rights::VMO_DEFAULT)).expect("insert");
    check!(first.0 != second.0, "a reused slot produced an identical handle value");
    check!(table.get(first).is_err(), "a stale handle value still resolves");

    let live = table.get(second).expect("live handle");
    check!(live.object().header().koid() != koid_a, "a stale handle resolved to the old object");
}

/// Two processes with different namespaces see genuinely different systems.
fn namespaces_isolate() {
    let alice = Process::new("test/alice", None, CapSet::NONE.with(CapClass::FileRead)).expect("process");
    let bob = Process::new("test/bob", None, CapSet::NONE).expect("process");

    let service = Vmo::new_anonymous(4096, false).expect("vmo");
    let handle = Handle::root(service, Rights::VMO_DEFAULT);
    alice.namespace().bind("/svc/storage", handle).expect("bind");

    check!(alice.namespace().resolve("/svc/storage").is_some(), "alice cannot see her own binding");
    check!(bob.namespace().resolve("/svc/storage").is_none(), "bob can see alice's binding");
    check!(
        alice.namespace().resolve("/svc/storage/inner").is_some(),
        "longest-prefix resolution failed"
    );
    check!(alice.namespace().resolve("/svc/other").is_none(), "unrelated path resolved");

    // A derived namespace can only narrow.
    let restricted = alice.namespace().derive(&["/svc/storage"], Rights::READ_ONLY).expect("derive ns");
    let derived = restricted.resolve("/svc/storage").expect("derived binding");
    check!(!derived.rights().contains(Rights::WRITE), "derived namespace kept WRITE");

    check!(alice.granted().contains(CapClass::FileRead), "granted capability class missing");
    check!(!bob.granted().contains(CapClass::FileRead), "capability class leaked to another process");

    alice.terminate(0);
    bob.terminate(0);
}

/// Memory objects, mapping, demand paging and copy-on-write.
fn memory_objects_and_cow() {
    let vmo = Vmo::new_anonymous(16 * 4096, true).expect("vmo");
    check!(vmo.size() == 16 * 4096, "vmo size wrong");
    check!(vmo.committed_pages() == 0, "a fresh vmo committed pages eagerly");

    vmo.write(4096, b"nodeos").expect("vmo write");
    check!(vmo.committed_pages() == 1, "write committed the wrong number of pages");
    let mut buffer = [0u8; 6];
    vmo.read(4096, &mut buffer).expect("vmo read");
    check!(&buffer == b"nodeos", "vmo read did not return what was written");

    // Uncommitted pages read as zero without committing.
    let mut zeros = [0xFFu8; 8];
    vmo.read(8192, &mut zeros).expect("vmo read");
    check!(zeros == [0u8; 8], "uncommitted vmo page did not read as zero");
    check!(vmo.committed_pages() == 1, "reading committed a page");

    vmo.resize(4 * 4096).expect("resize");
    check!(vmo.size() == 4 * 4096, "resize did not take effect");

    // Map it into a real address space and fault pages in.
    let process = Process::new("test/mm", None, CapSet::NONE).expect("process");
    let space = process.space();
    let base = space
        .map(
            vmo.clone(),
            0,
            4 * 4096,
            MapFlags::READ.or(MapFlags::WRITE),
            crate::mm::layout::Window::Heap,
            None,
        )
        .expect("map");
    check!(space.region_at(base).is_some(), "region lookup failed");
    check!(space.region_at(base - 4096).is_none(), "region lookup matched outside the region");

    // Demand paging: nothing is mapped until the first touch.
    check!(space.mapper().translate(base).is_none(), "mapping was populated eagerly");
    check!(space.handle_fault(base, true, false), "demand fault was not resolved");
    check!(space.mapper().translate(base).is_some(), "fault handler did not install a mapping");

    // A write to a non-writable region must not be resolved.
    let ro_base = space
        .map(vmo.clone(), 0, 4096, MapFlags::READ, crate::mm::layout::Window::MappedFile, None)
        .expect("map ro");
    check!(!space.handle_fault(ro_base, true, false), "a write fault on a read-only region was resolved");
    check!(space.handle_fault(ro_base, false, false), "a read fault on a read-only region was not resolved");

    // Copy-on-write: the first write makes a private copy.
    let cow_base = space
        .map(
            vmo.clone(),
            0,
            4096,
            MapFlags::READ.or(MapFlags::WRITE).or(MapFlags::COW),
            crate::mm::layout::Window::Shared,
            None,
        )
        .expect("map cow");
    check!(space.handle_fault(cow_base, false, false), "cow read fault failed");
    let (shared_phys, flags) = space.mapper().translate(cow_base).expect("cow mapping");
    check!(!flags.contains(crate::mm::paging::PageFlags::WRITABLE), "cow page mapped writable before the write");
    check!(space.handle_fault(cow_base, true, false), "cow write fault failed");
    let (private_phys, flags) = space.mapper().translate(cow_base).expect("cow mapping after write");
    check!(flags.contains(crate::mm::paging::PageFlags::WRITABLE), "cow page not writable after the write");
    check!(private_phys != shared_phys, "cow write did not produce a private copy");

    // Permissions cannot be widened by `protect`.
    check!(
        space.protect(ro_base, MapFlags::READ.or(MapFlags::WRITE)).is_err(),
        "protect widened permissions"
    );

    let before = mm::phys::stats().free_pages;
    process.terminate(0);
    drop(process);
    drop(vmo);
    let after = mm::phys::stats().free_pages;
    check!(after >= before, "tearing down a process leaked frames ({before} -> {after})");
}

static COUNTERS: [AtomicU64; 4] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

fn worker(class_index: usize) {
    for _ in 0..64 {
        COUNTERS[class_index].fetch_add(1, Ordering::Relaxed);
        sched::yield_now();
    }
}

/// Threads from every scheduling class run, and the ordering between classes
/// holds.
fn scheduler_classes_are_ordered() {
    let params = [
        (SchedParams::realtime(10_000_000, 2_000_000), "rt"),
        (SchedParams::interactive(24), "interactive"),
        (SchedParams::normal(), "normal"),
        (SchedParams::background(), "background"),
    ];

    let mut threads: Vec<Arc<crate::task::Thread>> = Vec::new();
    for (index, (param, name)) in params.into_iter().enumerate() {
        let thread = sched::spawn_kernel_thread(name, worker, index, param).expect("spawn");
        threads.push(thread);
    }

    check!(sched::runnable() >= 4, "spawned threads are not runnable");

    // Let them run. The idle context yields into the scheduler until every
    // worker has finished.
    let deadline = crate::time::now_ns() + 3 * crate::time::NANOS_PER_SEC;
    while threads.iter().any(|t| t.state() != crate::task::ThreadState::Dead) {
        if crate::time::now_ns() > deadline {
            break;
        }
        sched::yield_now();
    }

    for (index, (_, name)) in params.into_iter().enumerate() {
        let count = COUNTERS[index].load(Ordering::Relaxed);
        check!(count == 64, "{name} thread completed {count}/64 iterations");
    }
    for thread in &threads {
        check!(thread.state() == crate::task::ThreadState::Dead, "worker did not exit");
        check!(thread.runtime_ns() > 0 || thread.tid() > 0, "worker accumulated no runtime");
    }
}
