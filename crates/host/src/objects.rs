//! Kernel objects: the handles a guest waits on, signals and closes.
//!
//! A handle used to be a number with nothing behind it -- `CreateSemaphoreA`
//! returned a fresh heap address and `WaitForSingleObject` answered "signalled"
//! because nothing else in this machine could have been running to clear it. Once
//! a payload runs more than one guest thread that is no longer true: a worker's
//! semaphore has to hold a count, an event has to remember whether it is set, a
//! thread handle has to become signalled when that thread exits, and a wait on
//! anything unsignalled has to park the thread that asked.
//!
//! This module owns both halves of that: the objects, and the waits parked on
//! them -- which threads are waiting, on what, and until when. Which thread the
//! CPU runs next is [`crate::sched`]'s business; what a wait answers with is
//! decided here, so that a signal can complete a wait and report its result in
//! one step, before the woken thread has anywhere to run.

use std::collections::HashMap;

/// The two answers a wait gives.
pub const WAIT_OBJECT_0: u32 = 0;
pub const WAIT_TIMEOUT: u32 = 0x102;

/// What a handle names.
/// The owner field of a mutex that nobody holds. Thread 0 is the main thread, so
/// zero cannot mean "free": a lock taken by the first thread would read as
/// unheld and let every other thread in.
pub const NO_OWNER: u32 = u32::MAX;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Object {
    /// `CreateSemaphoreA(attributes, initial, maximum, name)`: a count a wait
    /// lowers and `ReleaseSemaphore` raises.
    Semaphore { count: u32, maximum: u32 },
    /// `CreateEventA(attributes, manualReset, initialState, name)`: a flag. The
    /// manual-reset kind stays set until `ResetEvent`; the automatic kind clears
    /// itself as soon as one wait takes it.
    Event { manual: bool, signalled: bool },
    /// `CreateMutexA(attributes, initialOwner, name)`: an owner, or
    /// [`NO_OWNER`] when it is free.
    Mutex { owner: u32 },
    /// A thread. Waits on it are answered once it has exited, and the exit code
    /// is what `GetExitCodeThread` reports.
    Thread {
        id: u32,
        exited: bool,
        exit_code: u32,
    },
    /// A handle with nothing behind it: the standard streams, an open file, a
    /// window's `EventW`. It is always signalled, which is what the guest sees
    /// when it waits on one.
    Always,
}

/// One thread's wait: what it is parked on, whether it needs all of them, and
/// when it gives up. A wait with no handles is a sleep.
#[derive(Clone, Debug)]
struct Waiter {
    handles: Vec<u32>,
    all: bool,
    /// In guest milliseconds, from the embedder's clock. `None` is `INFINITE`.
    deadline: Option<u64>,
}

/// The handle table and the threads parked on it.
#[derive(Default)]
pub struct Objects {
    objects: HashMap<u32, Object>,
    /// One wait per thread: a thread waits on one thing at a time.
    waiters: HashMap<u32, Waiter>,
    /// The threads parked on each handle, oldest first, so a signal serves the
    /// wait that has been outstanding longest.
    parked: HashMap<u32, Vec<u32>>,
    /// Handles handed out so far. Handle 0 is not one.
    next_handle: u32,
    /// The next thread id to hand out. Ids are never reused; see
    /// [`Objects::insert_thread`].
    next_thread_id: u32,
}

impl Objects {
    pub fn new() -> Self {
        // The main thread is 0, so the first spawned thread is 1.
        Self {
            next_thread_id: 1,
            ..Self::default()
        }
    }

    /// Creates a handle for `object`, distinct from every other.
    pub fn insert(&mut self, object: Object) -> u32 {
        self.next_handle += 1;
        self.objects.insert(self.next_handle, object);
        self.next_handle
    }

    /// Puts an object behind a handle the host chooses, rather than the next
    /// free number: a critical section is named by the address of the guest's
    /// own structure, so `EnterCriticalSection` and `LeaveCriticalSection`
    /// reach the same lock without a handle ever crossing the boundary.
    pub fn hold(&mut self, handle: u32, object: Object) {
        self.objects.insert(handle, object);
    }

    /// Creates a thread handle, together with the id the guest is told. The id is
    /// handed out here rather than by the session because the guest reads it back
    /// out of the `CreateThread` call, before the session has seen the thread.
    ///
    /// Ids are counted up and never reused. Everything the scheduler keeps per
    /// thread -- its suspend count, the wait it is parked on, which thread a
    /// woken answer belongs to -- is keyed by this id, so handing one out twice
    /// aliases two threads into one identity. Deriving the id from the highest
    /// one still in the handle table did exactly that: a guest that closes a
    /// thread's handle (or whose thread exits, dropping its object) brought the
    /// highest id back down, and the next `CreateThread` was told the number of a
    /// thread that was still running.
    pub fn insert_thread(&mut self) -> (u32, u32) {
        self.next_thread_id = self.next_thread_id.max(1);
        let id = self.next_thread_id;
        self.next_thread_id += 1;
        let handle = self.insert(Object::Thread {
            id,
            exited: false,
            exit_code: 0,
        });
        (handle, id)
    }

    pub fn get(&self, handle: u32) -> Option<Object> {
        self.objects.get(&handle).copied()
    }

    /// Closes a handle, with the waits parked on it: a guest that closes what
    /// another thread is waiting on has ended that wait, which is what the real
    /// kernel does when the last reference goes.
    pub fn close(&mut self, handle: u32) -> bool {
        if handle == 0 {
            return false;
        }
        if let Some(threads) = self.parked.remove(&handle) {
            for thread in threads {
                self.forget(thread);
            }
        }
        self.objects.remove(&handle).is_some()
    }

    /// Whether a wait on `handle` would be answered now, without taking it.
    pub fn signalled(&self, handle: u32) -> bool {
        match self.objects.get(&handle) {
            Some(Object::Semaphore { count, .. }) => *count > 0,
            Some(Object::Event { signalled, .. }) => *signalled,
            Some(Object::Mutex { owner }) => *owner == NO_OWNER,
            Some(Object::Thread { exited, .. }) => *exited,
            Some(Object::Always) => true,
            None => false,
        }
    }

    /// Whether a wait on `handle` by `thread` can be answered now, and takes the
    /// object when it can.
    ///
    /// A semaphore is lowered and an automatic event cleared by taking one; a
    /// manual event, an exited thread and a handle with no object behind it stay
    /// as they are, because any number of waits may see them.
    pub fn take(&mut self, handle: u32, thread: u32) -> bool {
        match self.objects.get_mut(&handle) {
            Some(Object::Semaphore { count, .. }) => {
                if *count == 0 {
                    return false;
                }
                *count -= 1;
                true
            }
            Some(Object::Event { signalled, manual }) => {
                if !*signalled {
                    return false;
                }
                if !*manual {
                    *signalled = false;
                }
                true
            }
            Some(Object::Mutex { owner }) => {
                if *owner != NO_OWNER && *owner != thread {
                    return false;
                }
                *owner = thread;
                true
            }
            Some(Object::Thread { exited, .. }) => *exited,
            Some(Object::Always) => true,
            None => false,
        }
    }

    /// Marks a semaphore, mutex or event as signalled. Answers whether the handle
    /// names something that can be signalled at all.
    pub fn signal(&mut self, handle: u32) -> bool {
        match self.objects.get_mut(&handle) {
            Some(Object::Event { signalled, .. }) => {
                *signalled = true;
                true
            }
            Some(Object::Semaphore { .. } | Object::Mutex { .. }) => true,
            _ => false,
        }
    }

    /// Raises a semaphore's count and lowers a mutex's. `ReleaseSemaphore` on the
    /// wrong handle is a guest bug; the count stays where it was.
    pub fn raise(&mut self, handle: u32, count: u32) -> bool {
        match self.objects.get_mut(&handle) {
            Some(Object::Semaphore { count: held, maximum }) => {
                *held = held.saturating_add(count).min(*maximum);
                true
            }
            Some(Object::Mutex { .. }) => true,
            _ => false,
        }
    }

    /// Records a thread's exit and answers the handles that become signalled
    /// because of it.
    pub fn thread_exited(&mut self, id: u32, code: u32) -> Vec<u32> {
        let mut signalled = Vec::new();
        for (handle, object) in self.objects.iter_mut() {
            if let Object::Thread {
                id: own,
                exited,
                exit_code,
            } = object
            {
                if *own == id {
                    *exited = true;
                    *exit_code = code;
                    signalled.push(*handle);
                }
            }
        }
        signalled
    }

    /// Whether these handles can be waited on at all. `WaitForMultipleObjects`
    /// refuses a bad handle with `WAIT_FAILED` rather than parking forever on a
    /// number that names nothing.
    pub fn known(&self, handles: &[u32]) -> bool {
        handles.iter().all(|handle| self.objects.contains_key(handle))
    }

    /// Tries a wait without parking: `Some(answer)` when it can be answered now,
    /// leaving the object taken.
    pub fn take_now(&mut self, thread: u32, handles: &[u32], all: bool) -> Option<u32> {
        let waiter = Waiter {
            handles: handles.to_vec(),
            all,
            deadline: None,
        };
        let saved = self.waiters.insert(thread, waiter);
        let answer = self.satisfy(thread);
        match answer {
            // The wait is over as soon as it is answered, so it must not stay
            // registered: a thread that goes on running with a wait on the books
            // reads as parked in every diagnostic, and `wake` would try to hand
            // it an answer it has already taken.
            Some(_) => self.forget(thread),
            None => match saved {
                Some(previous) => {
                    self.waiters.insert(thread, previous);
                }
                None => {
                    self.waiters.remove(&thread);
                }
            },
        }
        answer
    }

    /// Parks a thread on a wait. The caller has already seen that it cannot be
    /// answered now. The deadline is set by the session, which is the only part
    /// that knows the guest's clock.
    pub fn park(&mut self, thread: u32, handles: Vec<u32>, all: bool) {
        for handle in &handles {
            let parked = self.parked.entry(*handle).or_default();
            if !parked.contains(&thread) {
                parked.push(thread);
            }
        }
        self.waiters.insert(
            thread,
            Waiter {
                handles,
                all,
                deadline: None,
            },
        );
    }

    /// Gives a parked thread a deadline, in guest milliseconds.
    pub fn set_deadline(&mut self, thread: u32, deadline: u64) {
        if let Some(waiter) = self.waiters.get_mut(&thread) {
            waiter.deadline = Some(deadline);
        }
    }

    /// Gives up a mutex a thread owns, so the next wait can have it.
    pub fn release(&mut self, handle: u32, thread: u32) -> bool {
        match self.objects.get_mut(&handle) {
            Some(Object::Mutex { owner }) => {
                if *owner == thread {
                    *owner = NO_OWNER;
                }
                true
            }
            _ => false,
        }
    }

    /// Drops a thread's wait.
    pub fn forget(&mut self, thread: u32) {
        self.waiters.remove(&thread);
        for parked in self.parked.values_mut() {
            parked.retain(|waiter| *waiter != thread);
        }
        self.parked.retain(|_, parked| !parked.is_empty());
    }

    /// Hands a signalled object to the threads waiting on it, as many as it can
    /// serve: one unit of a semaphore per waiter, every waiter for a manual-reset
    /// event, all of them for a thread that has exited. Answers each woken thread
    /// with the value its wait returns.
    pub fn wake(&mut self, handle: u32) -> Vec<(u32, u32)> {
        let mut woken = Vec::new();
        let Some(threads) = self.parked.get(&handle).cloned() else {
            return woken;
        };
        for thread in threads {
            if !self.waiters.contains_key(&thread) {
                continue;
            }
            let Some(answer) = self.satisfy(thread) else {
                continue;
            };
            self.forget(thread);
            woken.push((thread, answer));
            if !self.signalled(handle) {
                break;
            }
        }
        woken
    }

    /// Answers the waits whose deadline has passed with `WAIT_TIMEOUT`.
    pub fn expire(&mut self, now: u64) -> Vec<(u32, u32)> {
        let expired: Vec<u32> = self
            .waiters
            .iter()
            .filter(|(_, waiter)| waiter.deadline.is_some_and(|deadline| deadline <= now))
            .map(|(thread, _)| *thread)
            .collect();
        for thread in &expired {
            self.forget(*thread);
        }
        expired
            .into_iter()
            .map(|thread| (thread, WAIT_TIMEOUT))
            .collect()
    }

    /// Whether any parked wait still has a deadline ahead of `now`.
    ///
    /// This is the run's reason to wait rather than to force: the deadline's
    /// own [`Self::expire`] will answer it once the guest's clock reaches it,
    /// and answering it sooner hands the guest progress it has not earned.
    pub fn any_deadline_after(&self, now: u64) -> bool {
        self.waiters
            .values()
            .any(|waiter| waiter.deadline.is_some_and(|deadline| deadline > now))
    }

    /// The waits that are parked, for the message a stall prints.
    pub fn waiting(&self) -> Vec<(u32, Vec<u32>)> {
        let mut waits: Vec<(u32, Vec<u32>)> = self
            .waiters
            .iter()
            .map(|(thread, waiter)| (*thread, waiter.handles.clone()))
            .collect();
        waits.sort_unstable_by_key(|(thread, _)| *thread);
        waits
    }

    /// Whether `thread`'s wait is satisfied, taking the objects it consumes and
    /// answering the value `WaitForSingleObject` returns for it.
    fn satisfy(&mut self, thread: u32) -> Option<u32> {
        let waiter = self.waiters.get(&thread)?.clone();
        if waiter.all {
            if waiter.handles.iter().all(|handle| self.signalled(*handle)) {
                for handle in &waiter.handles {
                    self.take(*handle, thread);
                }
                return Some(WAIT_OBJECT_0);
            }
            return None;
        }
        waiter
            .handles
            .iter()
            .position(|handle| self.take(*handle, thread))
            .map(|index| WAIT_OBJECT_0 + index as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn semaphore(objects: &mut Objects, count: u32, maximum: u32) -> u32 {
        objects.insert(Object::Semaphore { count, maximum })
    }

    #[test]
    fn a_thread_id_is_never_reused() {
        let mut objects = Objects::new();
        let (handle, id) = objects.insert_thread();
        assert_eq!(id, 1, "the main thread is 0, so the first spawn is 1");

        // The guest closes the handle it was given. The thread itself keeps
        // running, so its id must not come back to a later `CreateThread`.
        assert!(objects.close(handle));
        let (_, next) = objects.insert_thread();
        assert_ne!(next, id);
        assert_eq!(next, 2);
    }

    #[test]
    fn a_semaphore_serves_one_waiter_per_unit() {
        let mut objects = Objects::new();
        let handle = semaphore(&mut objects, 0, 4);
        objects.park(1, vec![handle], false);
        objects.park(2, vec![handle], false);

        // A count of zero serves nobody, which is exactly what a worker waiting
        // for its next job sits on.
        assert_eq!(objects.wake(handle), Vec::new());

        objects.raise(handle, 1);
        assert_eq!(objects.wake(handle), vec![(1, WAIT_OBJECT_0)]);
        assert_eq!(objects.wake(handle), Vec::new());

        objects.raise(handle, 2);
        assert_eq!(objects.wake(handle), vec![(2, WAIT_OBJECT_0)]);
    }

    #[test]
    fn a_manual_event_wakes_everyone_and_stays_set() {
        let mut objects = Objects::new();
        let handle = objects.insert(Object::Event {
            manual: true,
            signalled: false,
        });
        objects.park(1, vec![handle], false);
        objects.park(2, vec![handle], false);

        assert!(objects.signal(handle));
        assert_eq!(
            objects.wake(handle),
            vec![(1, WAIT_OBJECT_0), (2, WAIT_OBJECT_0)]
        );
        assert!(objects.take(handle, 3));
    }

    /// A sleep with a deadline still ahead of the guest's clock is the run's
    /// reason to wait rather than force: time itself is about to answer it.
    #[test]
    fn a_deadline_ahead_of_now_is_a_reason_to_wait() {
        let mut objects = Objects::new();
        assert!(!objects.any_deadline_after(1_000), "nothing parked at all");

        objects.park(1, Vec::new(), false);
        objects.set_deadline(1, 2_000);
        assert!(
            objects.any_deadline_after(1_000),
            "the deadline has not passed yet"
        );
        assert!(
            !objects.any_deadline_after(2_000),
            "at the deadline, expire() is what answers it, not a wait"
        );

        // An INFINITE sleep -- a worker parking for good -- is no reason to
        // wait: nothing will ever expire it.
        objects.park(2, Vec::new(), false);
        objects.set_deadline(2, u64::MAX);
        assert!(
            objects.any_deadline_after(3_000),
            "a second, later deadline still holds the run"
        );
        objects.forget(2);
        assert!(
            !objects.any_deadline_after(3_000),
            "a thread parked with no deadline never blocks on time"
        );
    }

    #[test]
    fn an_automatic_event_clears_itself() {
        let mut objects = Objects::new();
        let handle = objects.insert(Object::Event {
            manual: false,
            signalled: true,
        });

        assert!(objects.take(handle, 1));
        assert!(!objects.take(handle, 1));
    }

    #[test]
    fn waiting_on_several_handles_answers_with_the_one_that_fired() {
        let mut objects = Objects::new();
        let first = objects.insert(Object::Event {
            manual: false,
            signalled: false,
        });
        let second = objects.insert(Object::Event {
            manual: false,
            signalled: false,
        });

        assert_eq!(objects.take_now(1, &[first, second], false), None);
        objects.park(1, vec![first, second], false);
        assert!(objects.signal(second));
        assert_eq!(objects.wake(second), vec![(1, WAIT_OBJECT_0 + 1)]);
    }

    #[test]
    fn waiting_for_all_of_them_needs_every_one() {
        let mut objects = Objects::new();
        let first = semaphore(&mut objects, 0, 1);
        let second = semaphore(&mut objects, 0, 1);

        objects.park(1, vec![first, second], true);
        objects.raise(first, 1);
        assert_eq!(objects.wake(first), Vec::new());
        objects.raise(second, 1);
        assert_eq!(objects.wake(second), vec![(1, WAIT_OBJECT_0)]);
        assert!(!objects.signalled(first));
    }

    #[test]
    fn a_thread_handle_is_signalled_by_its_exit() {
        let mut objects = Objects::new();
        let (handle, id) = objects.insert_thread();

        assert!(!objects.take(handle, 1));
        assert_eq!(objects.thread_exited(id, 7), vec![handle]);
        assert!(objects.take(handle, 1));
        assert_eq!(
            objects.get(handle),
            Some(Object::Thread {
                id,
                exited: true,
                exit_code: 7
            })
        );
    }

    #[test]
    fn a_released_mutex_goes_to_the_waiting_thread() {
        let mut objects = Objects::new();
        let handle = objects.insert(Object::Mutex { owner: 1 });
        objects.park(2, vec![handle], false);

        assert!(objects.release(handle, 1));
        assert_eq!(objects.wake(handle), vec![(2, WAIT_OBJECT_0)]);
        assert_eq!(objects.get(handle), Some(Object::Mutex { owner: 2 }));
    }

    #[test]
    fn a_deadline_answers_with_a_timeout() {
        let mut objects = Objects::new();
        objects.park(1, Vec::new(), false);
        objects.set_deadline(1, 40);

        assert_eq!(objects.expire(39), Vec::new());
        assert_eq!(objects.expire(40), vec![(1, WAIT_TIMEOUT)]);
    }

    #[test]
    fn a_wait_answered_at_once_leaves_nothing_parked() {
        let mut objects = Objects::new();
        let handle = objects.insert(Object::Event {
            manual: true,
            signalled: true,
        });

        assert_eq!(objects.take_now(1, &[handle], false), Some(WAIT_OBJECT_0));
        assert!(objects.waiting().is_empty());
    }

    #[test]
    fn closing_a_handle_drops_its_waiters() {
        let mut objects = Objects::new();
        let handle = objects.insert(Object::Event {
            manual: false,
            signalled: false,
        });
        objects.park(1, vec![handle], false);

        assert_eq!(objects.waiting(), vec![(1, vec![handle])]);
        assert!(objects.close(handle));
        assert!(objects.waiting().is_empty());
        assert_eq!(objects.take_now(1, &[handle], false), None);
    }

    #[test]
    fn a_mutex_goes_to_one_owner_at_a_time() {
        let mut objects = Objects::new();
        let handle = objects.insert(Object::Mutex { owner: NO_OWNER });

        assert!(objects.take(handle, 1));
        assert!(!objects.take(handle, 2));
        assert!(objects.take(handle, 1));
    }
}
