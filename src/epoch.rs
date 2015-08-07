use std::cell::RefCell;
use std::mem;
use std::ptr;
use std::sync::atomic::{self, AtomicUsize, AtomicBool};
use std::sync::atomic::Ordering::{self, Relaxed, Acquire, Release, SeqCst};
use std::ops::{Deref, DerefMut};

use bag::Bag;
use cache_padded::CachePadded;

trait AnyType {}
impl<T: ?Sized> AnyType for T {}

struct GarbageBag(Vec<*mut AnyType>);

impl GarbageBag {
    fn new() -> GarbageBag {
        GarbageBag(vec![])
    }

    fn push(&mut self, elem: *mut AnyType) {
        self.0.push(elem)
    }

    fn len(&self) -> usize {
        self.0.len()
    }

    unsafe fn collect(&mut self) {
        for g in self.0.drain(..) {
            mem::drop(Box::from_raw(g))
        }
    }
}

unsafe impl Send for GarbageBag {}

struct LocalGarbage {
    old: GarbageBag,
    cur: GarbageBag,
}

impl LocalGarbage {
    fn new() -> LocalGarbage {
        LocalGarbage {
            old: GarbageBag::new(),
            cur: GarbageBag::new(),
        }
    }

    unsafe fn collect_one_epoch(&mut self) {
        self.old.collect();
        mem::swap(&mut self.old, &mut self.cur)
    }

    unsafe fn collect_all(&mut self) {
        self.old.collect();
        self.cur.collect();
    }
}

struct Participants {
    head: AtomicPtr<ParticipantNode>
}

type ParticipantNode = CachePadded<Participant>;

struct Participant {
    epoch: AtomicUsize,
    in_critical: AtomicUsize,
    active: AtomicBool,
    garbage: RefCell<LocalGarbage>,
    next: AtomicPtr<ParticipantNode>,
}

impl Participants {
    const fn new() -> Participants {
        Participants { head: AtomicPtr::new() }
    }

    fn enroll(&self) -> &'static Participant {
        let mut participant = Owned::new(unsafe { CachePadded::new(
            Participant {
                epoch: AtomicUsize::new(0),
                in_critical: AtomicUsize::new(0),
                active: AtomicBool::new(true),
                garbage: RefCell::new(LocalGarbage::new()),
                next: AtomicPtr::default(),
            }
        )});
        let g = Guard { _dummy: () };
        loop {
            let head = self.head.load(Relaxed, &g);
            unsafe { participant.next.store_shared(head, Relaxed) };
            match self.head.cas_and_ref(head, participant, Release, &g) {
                Ok(shared) => {
                    return unsafe { mem::transmute::<&Participant, _>(&shared) };
                }
                Err(owned) => {
                    participant = owned;
                }
            }
        }
    }

    fn iter<'a>(&'a self, g: &'a Guard) -> Iter<'a> {
        Iter {
            guard: g,
            next: &self.head,
            needs_acq: true,
        }
    }
}

struct Iter<'a> {
    guard: &'a Guard,
    next: &'a AtomicPtr<ParticipantNode>,
    needs_acq: bool,
}

impl<'a> Iterator for Iter<'a> {
    type Item = &'a Participant;
    fn next(&mut self) -> Option<&'a Participant> {
        let mut cur = if self.needs_acq {
            self.needs_acq = false;
            self.next.load(Acquire, self.guard)
        } else {
            self.next.load(Relaxed, self.guard)
        };

        while let Some(n) = cur {
            if !n.active.load(Relaxed) {
                cur = n.next.load(Relaxed, self.guard);
                let unlinked = unsafe { self.next.cas_shared(Some(n), cur, Relaxed) };
                if unlinked { self.guard.unlinked(n) }
                self.next = &n.next;
            } else {
                self.next = &n.next;
                return Some(&n)
            }
        }

        None
    }
}

struct EpochState {
    epoch: CachePadded<AtomicUsize>,
    garbage: [CachePadded<Bag<GarbageBag>>; 3],
    participants: Participants,
}

unsafe impl Send for EpochState {}
unsafe impl Sync for EpochState {}

impl EpochState {
    const fn new() -> EpochState {
        EpochState {
            epoch: CachePadded::zeroed(),
            garbage: [CachePadded::zeroed(),
                      CachePadded::zeroed(),
                      CachePadded::zeroed()],
            participants: Participants::new(),
        }
    }
}

static EPOCH: EpochState = EpochState::new();

impl Participant {
    fn enter(&self) {
        self.in_critical.store(self.in_critical.load(Relaxed) + 1, Relaxed);
        atomic::fence(SeqCst);

        let epoch = EPOCH.epoch.load(Relaxed);
        let delta = epoch - self.epoch.load(Relaxed);
        if delta > 0 {
            self.epoch.store(epoch, Relaxed);

            unsafe {
                if delta == 1 {
                    self.garbage.borrow_mut().collect_one_epoch();
                } else {
                    self.garbage.borrow_mut().collect_all();
                }
            }
        }
    }

    fn exit(&self) {
        self.in_critical.store(self.in_critical.load(Relaxed) - 1, Release);
    }

    fn reclaim<T>(&self, data: *mut T) {
        let data: *mut AnyType = data;
        self.garbage.borrow_mut().cur.push(unsafe {
            // forget any borrows within `data`:
            mem::transmute(data)
        });
    }
}

pub struct Owned<T> {
    data: Box<T>,
}

impl<T> Owned<T> {
    pub fn new(t: T) -> Owned<T> {
        Owned { data: Box::new(t) }
    }

    fn as_raw(&self) -> *mut T {
        self.deref() as *const _ as *mut _
    }
}

impl<T> Deref for Owned<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.data
    }
}

impl<T> DerefMut for Owned<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.data
    }
}

#[derive(PartialEq, Eq)]
pub struct Shared<'a, T: 'a> {
    data: &'a T,
}

impl<'a, T> Copy for Shared<'a, T> {}
impl<'a, T> Clone for Shared<'a, T> {
    fn clone(&self) -> Shared<'a, T> {
        Shared { data: self.data }
    }
}

impl<'a, T> Deref for Shared<'a, T> {
    type Target = &'a T;
    fn deref(&self) -> &&'a T {
        &self.data
    }
}

impl<'a, T> Shared<'a, T> {
    unsafe fn from_raw(raw: *mut T) -> Option<Shared<'a, T>> {
        if raw == ptr::null_mut() { None }
        else { Some(Shared { data: mem::transmute(raw) }) }
    }

    unsafe fn from_ref(r: &T) -> Shared<'a, T> {
        Shared { data: mem::transmute(r) }
    }

    unsafe fn from_owned(owned: Owned<T>) -> Shared<'a, T> {
        Shared::from_ref(owned.deref())
    }

    fn as_raw(&self) -> *mut T {
        self.data as *const _ as *mut _
    }
}

pub struct AtomicPtr<T> {
    ptr: atomic::AtomicPtr<T>,
}

impl<T> Default for AtomicPtr<T> {
    fn default() -> AtomicPtr<T> {
        AtomicPtr { ptr: atomic::AtomicPtr::new(ptr::null_mut()) }
    }
}

fn opt_shared_into_raw<T>(val: Option<Shared<T>>) -> *mut T {
    val.map(|p| p.as_raw()).unwrap_or(ptr::null_mut())
}

fn opt_owned_as_raw<T>(val: &Option<Owned<T>>) -> *mut T {
    val.as_ref().map(Owned::as_raw).unwrap_or(ptr::null_mut())
}

impl<T> AtomicPtr<T> {
    pub const fn new() -> AtomicPtr<T> {
        AtomicPtr { ptr: atomic::AtomicPtr::new(0 as *mut _) }
    }

    pub fn load<'a>(&self, ord: Ordering, _: &'a Guard) -> Option<Shared<'a, T>> {
        unsafe { Shared::from_raw(self.ptr.load(ord)) }
    }

    pub fn store(&self, val: Option<Owned<T>>, ord: Ordering) {
        self.ptr.store(opt_owned_as_raw(&val), ord)
    }

    pub fn store_and_ref<'a>(&self, val: Owned<T>, ord: Ordering, _: &'a Guard) -> Shared<'a, T> {
        unsafe {
            let shared = Shared::from_owned(val);
            self.store_shared(Some(shared), ord);
            shared
        }
    }

    pub unsafe fn store_shared(&self, val: Option<Shared<T>>, ord: Ordering) {
        self.ptr.store(opt_shared_into_raw(val), ord)
    }

    pub fn cas(&self, old: Option<Shared<T>>, new: Option<Owned<T>>, ord: Ordering)
               -> Result<(), Option<Owned<T>>>
    {
        if self.ptr.compare_and_swap(opt_shared_into_raw(old),
                                     opt_owned_as_raw(&new),
                                     ord) == opt_shared_into_raw(old)
        {
            Ok(())
        } else {
            Err(new)
        }
    }

    pub fn cas_and_ref<'a>(&self, old: Option<Shared<T>>, new: Owned<T>,
                           ord: Ordering, _: &'a Guard)
                           -> Result<Shared<'a, T>, Owned<T>>
    {
        if self.ptr.compare_and_swap(opt_shared_into_raw(old), new.as_raw(), ord)
            == opt_shared_into_raw(old)
        {
            Ok(unsafe { Shared::from_owned(new) })
        } else {
            Err(new)
        }
    }

    pub unsafe fn cas_shared(&self, old: Option<Shared<T>>, new: Option<Shared<T>>,
                             ord: Ordering)
                             -> bool
    {
        self.ptr.compare_and_swap(opt_shared_into_raw(old),
                                  opt_shared_into_raw(new),
                                  ord) == opt_shared_into_raw(old)
    }

    pub fn swap<'a>(&self, new: Option<Owned<T>>, ord: Ordering, _: &'a Guard)
                    -> Option<Shared<'a, T>> {
        unsafe { Shared::from_raw(self.ptr.swap(opt_owned_as_raw(&new), ord)) }
    }

    pub fn swap_shared<'a>(&self, new: Option<Shared<T>>, ord: Ordering, _: &'a Guard)
                           -> Option<Shared<'a, T>> {
        unsafe {
            Shared::from_raw(self.ptr.swap(opt_shared_into_raw(new), ord))
        }
    }
}

struct LocalEpoch {
    participant: &'static Participant,
}

impl LocalEpoch {
    fn new() -> LocalEpoch {
        LocalEpoch { participant: EPOCH.participants.enroll() }
    }
}

impl Drop for LocalEpoch {
    fn drop(&mut self) {
        pin().migrate_garbage();
        debug_assert!(self.participant.in_critical.load(Relaxed) == 0);
        self.participant.active.store(false, Relaxed);
    }
}

thread_local!(static LOCAL_EPOCH: LocalEpoch = LocalEpoch::new() );

pub struct Guard {
    _dummy: ()
}

static GC_THRESH: usize = 32;

pub fn pin() -> Guard {
    let needs_collect = LOCAL_EPOCH.with(|e| {
        e.participant.enter();
        e.participant.garbage.borrow().old.len() > GC_THRESH
    });
    let g = Guard {
        _dummy: ()
    };

    if needs_collect {
        g.try_collect();
    }

    g
}

impl Guard {
    pub fn unlinked<T>(&self, val: Shared<T>) {
        LOCAL_EPOCH.with(|e| e.participant.reclaim(val.as_raw()))
    }

    pub fn try_collect(&self) -> bool {
        let cur_epoch = EPOCH.epoch.load(SeqCst);

        for p in EPOCH.participants.iter(self) {
            if p.in_critical.load(Relaxed) > 0 && p.epoch.load(Relaxed) != cur_epoch {
                return false
            }
        }

        let new_epoch = cur_epoch.wrapping_add(1);
        atomic::fence(Acquire);
        if EPOCH.epoch.compare_and_swap(cur_epoch, new_epoch, SeqCst) != cur_epoch {
            return false
        }

        unsafe {
            for mut g in EPOCH.garbage[new_epoch.wrapping_add(1) % 3].iter_clobber() {
                g.collect();
            }

            LOCAL_EPOCH.with(|e| e.participant.garbage.borrow_mut().collect_one_epoch());
        }

        true
    }

    pub fn migrate_garbage(&self) {
        let mut old = GarbageBag::new();
        let mut cur = GarbageBag::new();
        let cur_epoch = LOCAL_EPOCH.with(|e| {
            mem::swap(&mut e.participant.garbage.borrow_mut().old, &mut old);
            mem::swap(&mut e.participant.garbage.borrow_mut().cur, &mut cur);
            e.participant.epoch.load(Relaxed)
        });

        EPOCH.garbage[cur_epoch.wrapping_sub(1) % 3].insert(old);
        EPOCH.garbage[cur_epoch].insert(cur);
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        LOCAL_EPOCH.with(|e| e.participant.exit());
    }
}

impl !Send for Guard {}
impl !Sync for Guard {}