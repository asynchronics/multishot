//! An async, lock-free, reusable channel for sending single values to
//! asynchronous tasks.
//!
//! In a multi-shot channel, the receiver half is reusable and able to recycle
//! the sender half without ever re-allocating. Sending, polling and recycling
//! the sender are all lock-free operations.
//!
//! # Example
//!
//!
//! ```
//! # use executor;
//! use std::thread;
//!
//! # executor::run(
//! async {
//!     let (s, mut r) = multishot::channel();
//!
//!     // Send a value to the channel from another thread.
//!     thread::spawn(move || {
//!         s.send("42");
//!     });
//!
//!     // Receive the value.
//!     let res = r.recv().await;
//!     assert_eq!(res, Ok("42"));
//!
//!     // Recycle the sender. This is guaranteed to succeed if the previous
//!     // message has been read.
//!     let s = r.sender().unwrap();
//!
//!     // Drop the sender on another thread without sending a message.
//!     thread::spawn(move || {
//!         drop(s);
//!     });
//!
//!     // Receive an error.
//!     let res = r.recv().await;
//!     assert_eq!(res, Err(multishot::RecvError {}));
//! }
//! # );
//! ```

#![warn(missing_docs, missing_debug_implementations, unreachable_pub)]

// The implementation of a reusable, lock-free, reallocation-free, one-shot
// channel is surprisingly tricky. In a regular, non-reusable one-shot channel,
// it is relatively easy to avoid missed notifications while also avoiding races
// between waker registration and waker invocation: it is enough for the sender
// to retrieve the waker only once the presence of a value has been signaled to
// the receiver, which implicitly signals at the same time the acquisition of a
// lock on the waker. This means, however, that the receiving side may read the
// value before the waker is consumed by the sender. This is a problem for a
// reusable channel as it means that the creation of a new sender after reading
// a value may block until the previous sender has indeed surrendered
// exclusivity on the waker.
//
// One workaround would be to allocate a new waker if the old sender still holds
// the lock on the previous waker, but such reallocation would somewhat defeat
// the idea of a reusable channel. The idea in this implementation is to instead
// signal the presence of a value only once the waker has been moved out from
// the waker slot, and then use the moved waker to wake the receiver task. This
// requires, however, giving the sender exclusivity on the waker before the
// value is sent. In order not to block a receiver attempting to register a new
// waker at the same time, the channel uses two waker slots so the receiver can
// always store a new waker in the unused slot. Missed notifications are avoided
// by having the sender check the availability of an updated waker before
// signaling the presence of a value.
//
// Despite the relative complexity of the state machine, the implementation is
// fairly efficient: it only uses 2 read-modify-write for polling operations (or
// none if the value is readily available) and typically 1 or 2
// read-modify-write for sending (depending whether a waker was registered or
// not). Compared to a non-reusable one-shot channel such as Tokio's, the only
// extra cost is 1 read-modify-write in the case a waker was registered before
// sending. Also, the implementation of `multishot` partially offsets this extra
// cost by using arithmetic atomic operations when sending rather than the
// typically more expensive compare-and-swap operations.
//
// Sending, receiving and recycling operations are lock-free; the last two are
// additionally wait-free.
//
// The state of the channel is tracked by the following bit flags:
//
// * OPEN [O]: the channel is open, both the receiver and the sender are alive
// * UPDATE [U]:
//    - if OPEN==1: an updated waker is available in the unused waker slot
//    - if OPEN==0 is observed by the receiver: a value is awaiting
// * INDEX [I]: index of the current waker slot (0 or 1)
//
// INDEX is reset to 0 whenever a sender is (re)created.
//
// Summary of possible states (excluding unobservable states)
//
// |  O  U  I  | Observer |                  Meaning                    |
// |-----------|----------|---------------------------------------------|
// |  0  0  0  |  Sender  |         channel closed by receiver          |
// |  0  0  0  | Receiver |     channel closed by sender, no value      |
// |  0  1  x  | Receiver |  channel closed by sender, value awaiting   |
// |  1  0  x  |   Any    |     a waker may be registered at index x    |
// |  1  1  x  |   Any    | an updated waker is registered at index !x  |
//
// NOTE: the O-U-I flag order is not the result of a subconscious French bias;
// it makes it possible to substitute a CAS by an arithmetic atomic operation in
// `Sender::send`, which is faster on some platforms.

mod loom_types;

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::marker::PhantomData;
use std::mem::{ManuallyDrop, MaybeUninit};
use std::panic::{RefUnwindSafe, UnwindSafe};
use std::pin::Pin;
use std::ptr::{self, NonNull};
use std::sync::atomic::Ordering;
use std::task::{Context, Poll, Waker};

use crate::loom_types::cell::UnsafeCell;
use crate::loom_types::sync::atomic::AtomicUsize;

// [I] Index of the current waker (0 or 1).
const INDEX: usize = 0b001;
// [U] Indicates whether an updated waker (case O==1) or a value (case O==0) are
// available.
const UPDATE: usize = 0b010;
// [O] Indicates whether the channel is open, i.e. whether both the receiver or
// the sender are alive.
const OPEN: usize = 0b100;

/// The shared data of `Receiver` and `Sender`.
struct Inner<T> {
    /// A bit field for `INDEX`, `UPDATE` and `OPEN`
    state: AtomicUsize,
    /// The value, if any.
    value: UnsafeCell<MaybeUninit<T>>,
    /// Redundant cells for the receiver waker.
    waker: [UnsafeCell<Option<Waker>>; 2],
}

impl<T> Inner<T> {
    // Sets the value without dropping the previous content.
    //
    // Safety: the caller must have exclusive access to the value.
    unsafe fn write_value(&self, t: T) {
        self.value.with_mut(|value| (&mut *value).write(t));
    }

    // Reads the value without moving it.
    //
    // Safety: the value must be initialized and the caller must have exclusive
    // access to the value. After the call, the value slot within `Inner` should
    // be considered uninitialized in order to avoid a double-drop.
    unsafe fn read_value(&self) -> T {
        self.value.with(|value| (*value).as_ptr().read())
    }

    // Drops the value in place without deallocation.
    //
    // Safety: the value must be initialized and the caller must have exclusive
    // access to the value.
    unsafe fn drop_value_in_place(&self) {
        self.value
            .with_mut(|value| ptr::drop_in_place((*value).as_mut_ptr()));
    }

    // Returns true if the waker at index `idx` and the waker provided as
    // argument would wake the same task.
    //
    // Safety: the caller must have exclusive access to the waker at index
    // `idx`.
    unsafe fn sender_will_wake(&self, idx: usize, other: &Waker) -> bool {
        match self.waker[idx].with(|waker| &(*waker)) {
            Some(waker) => waker.will_wake(other),
            None => false,
        }
    }

    // Sets the waker at index `idx`.
    //
    // Safety: the caller must have exclusive access to the waker at index
    // `idx`.
    unsafe fn set_waker(&self, idx: usize, new: Option<Waker>) {
        self.waker[idx].with_mut(|waker| (*waker) = new);
    }

    // Takes the waker out of the waker slot at index `idx`.
    //
    // Safety: the caller must have exclusive access to the waker at index
    // `idx`.
    unsafe fn take_waker(&self, idx: usize) -> Option<Waker> {
        self.waker[idx].with_mut(|waker| (*waker).take())
    }
}

/// Reusable receiver of a multi-shot channel.
///
/// A `Receiver` can be used to receive a value using the `Future` returned by
/// [`recv`](Receiver::recv). It can also produce a one-shot [`Sender`] with the
/// [`sender`](Receiver::sender) method, provided that there is currently no
/// live sender.
///
/// A receiver can be created with the [`channel`] function or with the
/// [`new`](Receiver::new) method.
#[derive(Debug)]
pub struct Receiver<T> {
    /// The shared data.
    inner: NonNull<Inner<T>>,
    /// Drop checker hint: we may drop an `Inner<T>`.
    _phantom: PhantomData<Inner<T>>,
}

impl<T> Receiver<T> {
    /// Creates a new receiver.
    pub fn new() -> Self {
        Self {
            inner: NonNull::new(Box::into_raw(Box::new(Inner {
                state: AtomicUsize::new(0),
                value: UnsafeCell::new(MaybeUninit::uninit()),
                waker: [UnsafeCell::new(None), UnsafeCell::new(None)],
            })))
            .unwrap(),
            _phantom: PhantomData,
        }
    }

    /// Returns a new sender if there is currently no live sender.
    ///
    /// This operation is wait-free. It is guaranteed to succeed (i) on its
    /// first invocation and (ii) on further invocations if the future returned
    /// by [`recv`](Receiver::recv) has been `await`ed (i.e. polled to
    /// completion) after the previous sender was created.
    pub fn sender(&mut self) -> Option<Sender<T>> {
        // A sender is created only if no sender is alive.
        //
        // Transitions:
        //
        // |  O  U  I  |  O  U  I  |
        // |-----------|-----------|
        // |  0  0  0  |  1  0  0  | -> Return Some(Sender)
        // |  0  1  x  |  1  0  0  | -> Return Some(Sender)
        // |  1  0  x  |  1  0  x  | -> Return None
        // |  1  1  x  |  1  1  x  | -> Return None

        // Ordering: This load synchronizes with the Release store in the
        // `Sender::send` method to ensure that the value (if any) is visible
        // and can be safely dropped.
        let state = self.inner().state.load(Ordering::Acquire);
        if state & OPEN == 0 {
            // Safety: the sender is consumed and the Acquire ordering on the
            // state ensures that all previous memory operations by the sender are
            // visible.
            Some(unsafe { self.sender_with_waker(state, None) })
        } else {
            None
        }
    }

    /// Receives a message asynchronously.
    ///
    /// If the channel is empty, the future returned by this method waits until
    /// there is a message. If there is no live sender and no message, the
    /// future completes and returns an error.
    pub fn recv(&mut self) -> Recv<'_, T> {
        Recv { receiver: self }
    }

    /// Get a reference to inner.
    fn inner(&self) -> &Inner<T> {
        // Safety: this is safe since `inner` is allocated for at least as long
        // as the receiver is alive.
        unsafe { self.inner.as_ref() }
    }

    /// Initialize the waker in slot 0, set the state to `OPEN` and return a sender.
    ///
    /// Safety: The sender must have been consumed and all memory operations by
    /// the sender on the value and on the waker must be visible in this thread.
    unsafe fn sender_with_waker(&mut self, state: usize, waker: Option<Waker>) -> Sender<T> {
        // Only create a sender if there is no live sender.
        debug_assert!(state & OPEN == 0);

        // If there is an unread value, drop it.
        if state & UPDATE == UPDATE {
            // Safety: the presence of an initialized value was just
            // checked and there is no risk of race since there is no live
            // sender.
            self.inner().drop_value_in_place();
        }

        // Set the waker in slot 0.
        self.inner().set_waker(0, waker);

        // Open the channel and set the current waker slot index to 0.
        //
        // Ordering: since the sender is created right now on this thread,
        // Relaxed ordering is sufficient.
        self.inner().state.store(OPEN, Ordering::Relaxed);

        Sender {
            inner: self.inner,
            _phantom: PhantomData,
        }
    }
}

unsafe impl<T: Send> Send for Receiver<T> {}
unsafe impl<T: Send> Sync for Receiver<T> {}

impl<T> UnwindSafe for Receiver<T> {}
impl<T> RefUnwindSafe for Receiver<T> {}

impl<T> Default for Receiver<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        // The drop handler clears the `OPEN`, `UPDATE` and `INDEX` flags. If
        // the channel was already closed by the sender, it drops the value (if
        // any) and deallocates `inner`.
        //
        // Transitions:
        //
        // |  O  U  I  |  O  U  I  |
        // |-----------|-----------|
        // |  0  0  0  | unobserv. | -> Deallocate
        // |  0  1  x  | unobserv. | -> Deallocate
        // |  1  0  x  |  0  0  0  |
        // |  1  1  x  |  0  0  0  |

        // Ordering: the value and wakers may need to be dropped prior to
        // deallocation in case the sender was dropped too, so Acquire ordering
        // is necessary to synchronize with the Release store in `Sender::send`.
        let state = self.inner().state.swap(0, Ordering::Acquire);

        // If the sender is alive, let it handle the cleanup.
        if state & OPEN == OPEN {
            return;
        }

        // Deallocate the channel since it was closed by the sender.
        //
        // Safety: `inner` will no longer be used once deallocated.
        unsafe {
            // If there is an unread value, drop it first.
            if state & UPDATE == UPDATE {
                // Safety: the presence of an initialized value was just
                // checked and there is no live receiver so no risk of race.
                self.inner().drop_value_in_place();
            }

            // Deallocate inner.
            drop(Box::from_raw(self.inner.as_ptr()));
        }
    }
}

/// Future returned by [`Receiver::recv()`].
#[derive(Debug)]
pub struct Recv<'a, T> {
    /// The shared data.
    receiver: &'a mut Receiver<T>,
}

impl<'a, T> Recv<'a, T> {
    /// Return Poll::Ready with either the value (if any) or an error.
    fn poll_complete(self: Pin<&mut Self>, state: usize) -> Poll<Result<T, RecvError>> {
        debug_assert!(state & OPEN == 0);
        let ret = if state & UPDATE == UPDATE {
            // Safety: the presence of an initialized value was just checked
            // and there is no live sender so no risk of race.
            let value = unsafe { self.receiver.inner().read_value() };

            Ok(value)
        } else {
            Err(RecvError {})
        };

        Poll::Ready(ret)
    }
}

impl<'a, T> Future for Recv<'a, T> {
    type Output = Result<T, RecvError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // The poll method proceeds in two steps. In the first step, the
        // `UPDATE` flag is cleared to make sure that a concurrent sender
        // operation does not try to access the waker update slot while a new
        // waker is being registered. The `UPDATE` flag is then set in Step 2
        // once the new waker is registered, checking at the same time whether
        // the sender has not been consumed while the new waker was being
        // registered. This check is necessary because if it was consumed, the
        // sender may not have been able to send a notification (if the current
        // waker slot was empty) or may have sent one using an outdated waker.
        //
        // Transitions:
        //
        // Step 1
        //
        // |  O  U  I  |  O  U  I  |
        // |-----------|-----------|
        // |  0  0  0  |  0  0  0  | -> Return Ready(Error)
        // |  0  1  x  |  0  0  x  | -> Return Ready(Message)
        // |  1  0  x  |  1  0  x  | -> Step 2
        // |  1  1  x  |  1  0  x  | -> Step 2
        //
        // Step 2
        //
        // |  O  U  I  |  O  U  I  |
        // |-----------|-----------|
        // |  0  0  x  |  0  0  0  | -> Return Ready(Error)
        // |  0  1  x  |  0  0  0  | -> Return Ready(Message)
        // |  1  0  x  |  1  1  x  | -> Return Pending

        // Fast path: this is an optimization in case the sender has already
        // been consumed and has closed the channel.
        let state = self.receiver.inner().state.load(Ordering::Acquire);
        if state & OPEN == 0 {
            return self.poll_complete(state);
        }

        // Prevent the sender from updating the current waker slot.
        //
        // Ordering: Acquire ordering is necessary since some member data may be
        // read and/or modified after reading the state.
        let state = self
            .receiver
            .inner()
            .state
            .fetch_and(!UPDATE, Ordering::Acquire);

        // Check whether the sender has closed the channel.
        if state & OPEN == 0 {
            return self.poll_complete(state);
        }

        // Register the new waker in the unused waker slot.
        let idx = !state & INDEX;
        unsafe {
            // Safety: the sender thread never accesses the waker stored in the
            // slot not pointed to by `INDEX` and it does not modify `INDEX` as
            // long as the `UPDATE` flag is cleared.
            if !self.receiver.inner().sender_will_wake(idx, cx.waker()) {
                self.receiver
                    .inner()
                    .set_waker(idx, Some(cx.waker().clone()));
            }
        }

        // Make the new waker visible to the sender.
        //
        // Ordering: the waker may have been modified above so Release ordering
        // is necessary to synchronize with the Acquire load in `Sender::send`
        // or `Sender::drop`. Acquire ordering is also necessary since the
        // message may be loaded.
        let state = self
            .receiver
            .inner()
            .state
            .fetch_or(UPDATE, Ordering::AcqRel);

        // It is necessary to check again whether the sender has closed the
        // channel, because if it did, the sender may not have been able to send
        // a notification (if the current waker slot was empty) or may have sent
        // one using an outdated waker.
        if state & OPEN == 0 {
            // The `OPEN` flag should not have been set in the above `swap` so
            // it needs to be cleared again.
            //
            // Ordering: Relaxed is enough since the receiver was dropped and
            // therefore no other thread can observe the state.
            self.receiver.inner().state.store(0, Ordering::Relaxed);

            return self.poll_complete(state);
        }

        Poll::Pending
    }
}

/// Single-use sender of a multi-shot channel.
///
/// A `Sender` can be created with the [`channel`]  function or by recycling a
/// previously consumed sender with the [`Receiver::sender`] method.
#[derive(Debug)]
pub struct Sender<T> {
    /// The shared data.
    inner: NonNull<Inner<T>>,
    /// Drop checker hint: we may drop an `Inner<T>` and thus a `T`.
    _phantom: PhantomData<Inner<T>>,
}

impl<T> Sender<T> {
    /// Sends a value to the receiver and consume the sender.
    pub fn send(self, t: T) {
        // The send method is iterative. At each iteration, the `UPDATE` flag is
        // checked. If not set, the presence of a value is signaled immediately
        // and the function returns after sending a notification. If the
        // `UPDATE` flag is set, it is cleared and the index of the current
        // waker becomes that of the redundant slot in the next iteration.
        //
        // Transitions:
        //
        // |  O  U  I  |  O  U  I  |
        // |-----------|-----------|
        // |  0  0  0  | unobserv. | -> Deallocate
        // |  1  0  x  |  0  1 !x  | -> End
        // |  1  1  x  |  1  0 !x  | -> Retry

        // It is necessary to make sure that the destructor is not run: since
        // the `OPEN` flag is cleared once the value is sent, `Sender::drop`
        // would otherwise wrongly consider the channel as closed by the
        // receiver and would deallocate `inner` while the receiver is alive.
        let this = ManuallyDrop::new(self);

        // Store the value.
        //
        // Note: there is no need to drop a previous value since the
        // `Receiver::sender` method would have dropped the value if there was
        // one.
        //
        // Safety: no race for accessing the value is possible since there can
        // be at most one sender alive at a time and the receiver will not read
        // the value before the `OPEN` flag is cleared.
        unsafe { this.inner().write_value(t) };

        // The current waker is always initially at index 0.
        let mut idx = 0;

        loop {
            // Take the current waker.
            //
            // Safety: the receiver thread never accesses the waker stored at
            // `INDEX`.
            let waker = unsafe { this.inner().take_waker(idx) };

            // There is some small trickery here: the `state` is decremented by
            // the numeric value of `UPDATE` (0b010) in order to either clear
            // `UPDATE` if it was set, or to set `UPDATE` and clear `OPEN` (i.e.
            // signal the presence of a value) if `UPDATE` wasn't set. In order
            // to flip the value of the current waker index within the same
            // `fetch_sub` operation, the value (idx - !idx) is added to the
            // decrement, which when added to 0b010 simplifies to the below
            // expression of the decrement. Even though flipping the waker index
            // is not justified when there is no waker update, it is simpler to
            // do so and does no harm since in such cases the waker will not be
            // used anymore.
            //
            // Ordering: Acquire is necessary to synchronize with the Release
            // store in the `Receiver::poll` method in case an updated waker
            // needs to be taken. Release is in turn necessary to ensure the
            // visibility of both (i) the value and (ii) the consumption of the
            // waker in the previous loop iteration (if any). The Release
            // synchronizes with the Acquire load of the state in the receiver
            // `poll` and `sender` methods.
            let decrement = 0b001 + (idx << 1);
            let state = this.inner().state.fetch_sub(decrement, Ordering::AcqRel);

            // Deallocate the channel if closed.
            //
            // Safety: `inner` will no longer be used once deallocated. In
            // particular, the destructor will not be called.
            unsafe {
                if state & OPEN == 0 {
                    // Safety: a value was just written and there is no live
                    // receiver so no risk of a race.
                    this.inner().drop_value_in_place();

                    drop(Box::from_raw(this.inner.as_ptr()));

                    return;
                }
            }

            // Unless the waker was updated, notify the receiver and return.
            if state & UPDATE == 0 {
                if let Some(waker) = waker {
                    waker.wake();
                }

                return;
            }

            // Update the local waker index to the current value of `INDEX`.
            idx = 1 - idx;
        }
    }

    /// Get a reference to inner.
    fn inner(&self) -> &Inner<T> {
        // Safety: this is safe since `inner` is allocated for at least as long
        // as the sender is alive.
        unsafe { self.inner.as_ref() }
    }
}

unsafe impl<T: Send> Send for Sender<T> {}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        // The drop handler is iterative. At each iteration, the `UPDATE` flag
        // is checked. If not set, the closure of the channel is signaled
        // immediately and the function returns after sending a notification. If
        // the `UPDATE` flag is set, it is cleared and the index of the current
        // waker becomes that of the redundant slot in the next iteration.
        //
        // Transitions:
        //
        // |  O  U  I  |  O  U  I  |
        // |-----------|-----------|
        // |  0  0  0  | unobserv. | -> Deallocate
        // |  1  0  x  |  0  0  0  | -> End
        // |  1  1  x  |  1  0 !x  | -> Retry

        // The current waker is always initially at index 0.
        let mut idx = 0;
        // There cannot be a registered waker at index 0 at this point so it is
        // set to `None`.
        let mut waker: Option<Waker> = None;

        let mut state = self.inner().state.load(Ordering::Relaxed);
        loop {
            // Modify the state according to the transition table.
            let new_state = if state & UPDATE == 0 {
                0
            } else {
                state ^ (UPDATE | INDEX)
            };

            // Ordering: Acquire is necessary to synchronize with the Release
            // store in the `Receiver::poll` method in case an updated waker
            // needs to be taken or if the channel was closed and the wakers
            // need to be dropped. Release is in turn necessary to ensure the
            // visibility of the consumption of the waker in the previous loop
            // iteration (if any). The Release synchronizes with the Acquire
            // load of the state in the receiver `poll` and `sender` methods.
            match self.inner().state.compare_exchange_weak(
                state,
                new_state,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(s) => state = s,
                Err(s) => {
                    state = s;
                    continue;
                }
            }

            // Deallocate the channel if closed.
            //
            // Safety: `inner` will no longer be used once deallocated.
            unsafe {
                if state & OPEN == 0 {
                    drop(Box::from_raw(self.inner.as_ptr()));

                    return;
                }
            }

            // Unless the waker was updated, notify the receiver and return.
            if state & UPDATE == 0 {
                if let Some(waker) = waker {
                    waker.wake();
                }

                return;
            }

            // Update the local waker index to the current value of `INDEX`.
            idx = 1 - idx;

            // Take the current waker.
            //
            // Safety: the receiver thread never accesses the waker stored at
            // `INDEX`.
            waker = unsafe { self.inner().take_waker(idx) };
        }
    }
}

/// Error signaling that the sender was dropped without sending a value.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct RecvError {}

impl fmt::Display for RecvError {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(fmt, "channel closed")
    }
}

impl Error for RecvError {}

/// Creates a new multi-shot channel.
pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
    let mut receiver = Receiver::new();
    let sender = receiver.sender().unwrap();

    (sender, receiver)
}

#[cfg(all(test, not(multishot_loom)))]
mod tests {
    use super::*;

    use std::future::Future;
    use std::sync::Arc;
    use std::task::{Context, Poll, Wake};

    // Dumb waker counting notifications.
    struct TestWaker {
        count: AtomicUsize,
    }
    impl TestWaker {
        fn new() -> Self {
            Self {
                count: AtomicUsize::new(0),
            }
        }
        fn take_count(&self) -> usize {
            self.count.swap(0, Ordering::Acquire)
        }
    }
    impl Wake for TestWaker {
        fn wake(self: Arc<Self>) {
            self.count.fetch_add(1, Ordering::Release);
        }
    }

    #[cfg(not(miri))]
    fn spawn_and_join<F>(f: F)
    where
        F: FnOnce(),
        F: Send + 'static,
    {
        let th = std::thread::spawn({
            move || {
                f();
            }
        });
        th.join().unwrap();
    }

    #[cfg(miri)]
    // MIRI does not support threads, so just execute sequentially.
    fn spawn_and_join<F>(f: F)
    where
        F: FnOnce(),
        F: Send + 'static,
    {
        f();
    }

    // Executes a closure consuming the sender and checks the result of the
    // completed future.
    fn multishot_notify<F>(f: F, expect: Result<i32, RecvError>)
    where
        F: FnOnce(Sender<i32>) + Send + Copy + 'static,
    {
        let test_waker = Arc::new(TestWaker::new());
        let waker = test_waker.clone().into();
        let mut cx = Context::from_waker(&waker);
        let mut receiver: Receiver<i32> = Receiver::new();

        // Consume sender before polling.
        {
            let sender = receiver.sender().expect("could not create sender");
            let mut fut = receiver.recv();
            let mut fut = Pin::new(&mut fut);

            spawn_and_join(move || f(sender));
            let res = fut.as_mut().poll(&mut cx);
            assert_eq!(test_waker.take_count(), 0);
            assert_eq!(res, Poll::Ready(expect));
        }

        // Consume sender after polling.
        {
            let sender = receiver.sender().expect("could not create sender");
            let mut fut = receiver.recv();
            let mut fut = Pin::new(&mut fut);

            let res = fut.as_mut().poll(&mut cx);
            assert_eq!(res, Poll::Pending);
            spawn_and_join(move || f(sender));
            assert_eq!(test_waker.take_count(), 1);
            let res = fut.as_mut().poll(&mut cx);
            assert_eq!(res, Poll::Ready(expect));
        }
    }

    // Changes the waker before executing a closure consuming the sender.
    fn multishot_change_waker<F>(f: F, expect: Result<i32, RecvError>)
    where
        F: FnOnce(Sender<i32>) + Send + Copy + 'static,
    {
        let test_waker1 = Arc::new(TestWaker::new());
        let waker1 = test_waker1.clone().into();
        let mut cx1 = Context::from_waker(&waker1);
        let test_waker2 = Arc::new(TestWaker::new());
        let waker2 = test_waker2.clone().into();
        let mut cx2 = Context::from_waker(&waker2);

        // Change waker and consume sender.
        {
            let (sender, mut receiver) = channel::<i32>();
            let mut fut = receiver.recv();
            let mut fut = Pin::new(&mut fut);

            let res = fut.as_mut().poll(&mut cx1);
            assert_eq!(res, Poll::Pending);
            let res = fut.as_mut().poll(&mut cx2);
            assert_eq!(res, Poll::Pending);
            spawn_and_join(move || f(sender));
            assert_eq!(test_waker2.take_count(), 1);
            let res = fut.as_mut().poll(&mut cx1);
            assert_eq!(test_waker1.take_count(), 0);
            assert_eq!(test_waker2.take_count(), 0);
            assert_eq!(res, Poll::Ready(expect));
        }

        // Change waker twice and consume sender.
        {
            let (sender, mut receiver) = channel::<i32>();
            let mut fut = receiver.recv();
            let mut fut = Pin::new(&mut fut);

            let res = fut.as_mut().poll(&mut cx1);
            assert_eq!(res, Poll::Pending);
            let res = fut.as_mut().poll(&mut cx2);
            assert_eq!(res, Poll::Pending);
            let res = fut.as_mut().poll(&mut cx1);
            assert_eq!(res, Poll::Pending);
            spawn_and_join(move || f(sender));
            assert_eq!(test_waker1.take_count(), 1);
            let res = fut.as_mut().poll(&mut cx2);
            assert_eq!(test_waker1.take_count(), 0);
            assert_eq!(test_waker2.take_count(), 0);
            assert_eq!(res, Poll::Ready(expect));
        }
    }
    #[test]
    /// Sends a message.
    fn multishot_send_notify() {
        multishot_notify(|sender| sender.send(42), Ok(42));
    }
    #[test]
    /// Drops the sender.
    fn multishot_drop_notify() {
        multishot_notify(|sender| drop(sender), Err(RecvError {}));
    }
    #[test]
    /// Sends a message after changing the waker.
    fn multishot_send_change_waker() {
        multishot_change_waker(|sender| sender.send(42), Ok(42));
    }
    #[test]
    /// Drops the sender after changing the waker.
    fn multishot_drop_change_waker() {
        multishot_change_waker(|sender| drop(sender), Err(RecvError {}));
    }
}

#[cfg(all(test, multishot_loom))]
mod tests {
    use super::*;

    use std::future::Future;
    use std::sync::Arc;
    use std::task::{Context, Poll, Wake};

    use loom::sync::atomic::{AtomicBool, AtomicUsize};
    use loom::thread;

    // Dumb waker counting notifications.
    struct TestWaker {
        count: AtomicUsize,
    }
    impl TestWaker {
        fn new() -> Self {
            Self {
                count: AtomicUsize::new(0),
            }
        }
        fn take_count(&self) -> usize {
            self.count.swap(0, Ordering::Acquire)
        }
    }
    impl Wake for TestWaker {
        fn wake(self: Arc<Self>) {
            self.count.fetch_add(1, Ordering::Release);
        }
    }

    // Executes a closure consuming the sender and checks the result of the
    // completed future.
    fn multishot_loom_notify<F>(f: F, expect: Result<i32, RecvError>)
    where
        F: FnOnce(Sender<i32>) + Send + Sync + Copy + 'static,
    {
        loom::model(move || {
            let test_waker = Arc::new(TestWaker::new());
            let waker = test_waker.clone().into();
            let mut cx = Context::from_waker(&waker);

            let (sender, mut receiver) = channel();

            let has_message = Arc::new(AtomicBool::new(false));
            thread::spawn({
                let has_message = has_message.clone();
                move || {
                    f(sender);
                    has_message.store(true, Ordering::Release);
                }
            });

            let mut fut = receiver.recv();
            let mut fut = Pin::new(&mut fut);

            let res = fut.as_mut().poll(&mut cx);

            match res {
                Poll::Pending => {
                    let msg = has_message.load(Ordering::Acquire);
                    let event_count = test_waker.take_count();
                    if event_count == 0 {
                        // Make sure that if the waker was not notified, then no
                        // message was sent (or equivalently, if a message was sent,
                        // the waker was notified).
                        assert_eq!(msg, false);
                    } else {
                        assert_eq!(event_count, 1);
                        // Make sure that if the waker was notified, the message
                        // can be retrieved (this is crucial to ensure that
                        // notifications are not lost).
                        let res = fut.as_mut().poll(&mut cx);
                        assert_eq!(test_waker.take_count(), 0);
                        assert_eq!(res, Poll::Ready(expect));
                    }
                }
                Poll::Ready(val) => {
                    assert_eq!(val, expect);
                }
            }
        });
    }

    // Executes a closure consuming the sender and checks the result of the
    // completed future, changing the waker several times.
    fn multishot_loom_change_waker<F>(f: F, expect: Result<i32, RecvError>)
    where
        F: FnOnce(Sender<i32>) + Send + Sync + Copy + 'static,
    {
        loom::model(move || {
            let test_waker1 = Arc::new(TestWaker::new());
            let waker1 = test_waker1.clone().into();
            let mut cx1 = Context::from_waker(&waker1);

            let test_waker2 = Arc::new(TestWaker::new());
            let waker2 = test_waker2.clone().into();
            let mut cx2 = Context::from_waker(&waker2);

            let (sender, mut receiver) = channel();

            thread::spawn({
                move || {
                    f(sender);
                }
            });

            let mut fut = receiver.recv();
            let mut fut = Pin::new(&mut fut);

            // Attempt to poll the future to completion with the provided context.
            fn try_complete(
                fut: &mut Pin<&mut Recv<i32>>,
                cx: &mut Context,
                other_cx: &mut Context,
                test_waker: &TestWaker,
                other_test_waker: &TestWaker,
                expect: Result<i32, RecvError>,
            ) -> bool {
                let res = fut.as_mut().poll(cx);

                // If `Ready` is returned we are done.
                if let Poll::Ready(val) = res {
                    assert_eq!(val, expect);
                    return true;
                }

                // The sender should not have used the other waker even if it
                // was registered before the last call to `poll`.
                assert_eq!(other_test_waker.take_count(), 0);

                // Although the last call to `poll` has returned `Pending`, the
                // sender may have been consumed in the meantime so check
                // whether there is a notification.
                let event_count = test_waker.take_count();
                if event_count != 0 {
                    // Expect only one notification.
                    assert_eq!(event_count, 1);

                    // Since the task was notified it is expected that the
                    // future is now ready.
                    let res = fut.as_mut().poll(other_cx);
                    assert_eq!(test_waker.take_count(), 0);
                    assert_eq!(other_test_waker.take_count(), 0);
                    assert_eq!(res, Poll::Ready(expect));
                    return true;
                }

                // The future was not polled to completion.
                false
            }

            // Poll with cx1.
            if try_complete(
                &mut fut,
                &mut cx1,
                &mut cx2,
                &test_waker1,
                &test_waker2,
                expect,
            ) {
                return;
            }
            // Poll with cx2.
            if try_complete(
                &mut fut,
                &mut cx2,
                &mut cx1,
                &test_waker2,
                &test_waker1,
                expect,
            ) {
                return;
            }
            // Poll again with cx1.
            if try_complete(
                &mut fut,
                &mut cx1,
                &mut cx2,
                &test_waker1,
                &test_waker2,
                expect,
            ) {
                return;
            }
        });
    }

    // Executes a closure consuming the sender and attempts to reuse the
    // channel.
    fn multishot_loom_recycle<F>(f: F)
    where
        F: FnOnce(Sender<i32>) + Send + Sync + Copy + 'static,
    {
        loom::model(move || {
            let test_waker = Arc::new(TestWaker::new());
            let waker = test_waker.clone().into();
            let mut cx = Context::from_waker(&waker);

            let (sender, mut receiver) = channel();

            {
                thread::spawn({
                    move || {
                        f(sender);
                    }
                });

                let mut fut = receiver.recv();
                let mut fut = Pin::new(&mut fut);

                // Poll up to twice.
                let res = fut.as_mut().poll(&mut cx);
                if res == Poll::Pending {
                    let res = fut.as_mut().poll(&mut cx);
                    if res == Poll::Pending {
                        return;
                    }
                }
            }

            // The future was polled to completion, meaning that the sender was
            // consumed and should be immediately recyclable.
            let sender = receiver
                .sender()
                .expect("Could not recycle the sender after it was consumed");

            // It's all downhill from here, just make sure te recycled sender
            // works correctly.
            {
                thread::spawn({
                    move || {
                        sender.send(13);
                    }
                });

                let mut fut = receiver.recv();
                let mut fut = Pin::new(&mut fut);

                let res = fut.as_mut().poll(&mut cx);
                if let Poll::Ready(val) = res {
                    assert_eq!(val, Ok(13));
                }
            }
        });
    }

    #[test]
    /// Sends a message.
    fn multishot_loom_send_notify() {
        multishot_loom_notify(|sender| sender.send(42), Ok(42));
    }
    #[test]
    /// Drops the sender.
    fn multishot_loom_drop_notify() {
        multishot_loom_notify(|sender| drop(sender), Err(RecvError {}));
    }
    #[test]
    /// Changes the waker while sending a message.
    fn multishot_loom_send_change_waker() {
        multishot_loom_change_waker(|sender| sender.send(42), Ok(42));
    }
    #[test]
    /// Changes the waker while dropping the sender.
    fn multishot_loom_drop_change_waker() {
        multishot_loom_change_waker(|sender| drop(sender), Err(RecvError {}));
    }
    #[test]
    /// Recycles the sender after sending a message.
    fn multishot_loom_send_recycle() {
        multishot_loom_recycle(|sender| sender.send(42));
    }
    #[test]
    /// Recycles the sender after dropping the previous sender.
    fn multishot_loom_drop_recycle() {
        multishot_loom_recycle(|sender| drop(sender));
    }
}
