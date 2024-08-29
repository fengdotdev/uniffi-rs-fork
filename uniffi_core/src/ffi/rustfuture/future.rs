/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! [`RustFuture`] represents a [`Future`] that can be sent to the foreign code over FFI.
//!
//! This type is not instantiated directly, but via the procedural macros, such as `#[uniffi::export]`.
//!
//! # The big picture
//!
//! We implement async foreign functions using a simplified version of the Future API:
//!
//! 0. At startup, register a [RustFutureContinuationCallback] by calling
//!    rust_future_continuation_callback_set.
//! 1. Call the scaffolding function to get a [Handle]
//! 2a. In a loop:
//!   - Call [rust_future_poll]
//!   - Suspend the function until the [rust_future_poll] continuation function is called
//!   - If the continuation was function was called with [RustFuturePoll::Ready], then break
//!     otherwise continue.
//! 2b. If the async function is cancelled, then call [rust_future_cancel].  This causes the
//!     continuation function to be called with [RustFuturePoll::Ready] and the [RustFuture] to
//!     enter a cancelled state.
//! 3. Call [rust_future_complete] to get the result of the future.
//! 4. Call [rust_future_free] to free the future, ideally in a finally block.  This:
//!    - Releases any resources held by the future
//!    - Calls any continuation callbacks that have not been called yet
//!
//! Note: Technically, the foreign code calls the scaffolding versions of the `rust_future_*`
//! functions.  These are generated by the scaffolding macro, specially prefixed, and extern "C",
//! and manually monomorphized in the case of [rust_future_complete].  See
//! `uniffi_macros/src/setup_scaffolding.rs` for details.
//!
//! ## How does `Future` work exactly?
//!
//! A [`Future`] in Rust does nothing. When calling an async function, it just
//! returns a `Future` but nothing has happened yet. To start the computation,
//! the future must be polled. It returns [`Poll::Ready(r)`][`Poll::Ready`] if
//! the result is ready, [`Poll::Pending`] otherwise. `Poll::Pending` basically
//! means:
//!
//! > Please, try to poll me later, maybe the result will be ready!
//!
//! This model is very different than what other languages do, but it can actually
//! be translated quite easily, fortunately for us!
//!
//! But… wait a minute… who is responsible to poll the `Future` if a `Future` does
//! nothing? Well, it's _the executor_. The executor is responsible _to drive_ the
//! `Future`: that's where they are polled.
//!
//! But… wait another minute… how does the executor know when to poll a [`Future`]?
//! Does it poll them randomly in an endless loop? Well, no, actually it depends
//! on the executor! A well-designed `Future` and executor work as follows.
//! Normally, when [`Future::poll`] is called, a [`Context`] argument is
//! passed to it. It contains a [`Waker`]. The [`Waker`] is built on top of a
//! [`RawWaker`] which implements whatever is necessary. Usually, a waker will
//! signal the executor to poll a particular `Future`. A `Future` will clone
//! or pass-by-ref the waker to somewhere, as a callback, a completion, a
//! function, or anything, to the system that is responsible to notify when a
//! task is completed. So, to recap, the waker is _not_ responsible for waking the
//! `Future`, it _is_ responsible for _signaling_ the executor that a particular
//! `Future` should be polled again. That's why the documentation of
//! [`Poll::Pending`] specifies:
//!
//! > When a function returns `Pending`, the function must also ensure that the
//! > current task is scheduled to be awoken when progress can be made.
//!
//! “awakening” is done by using the `Waker`.
//!
//! [`Future`]: https://doc.rust-lang.org/std/future/trait.Future.html
//! [`Future::poll`]: https://doc.rust-lang.org/std/future/trait.Future.html#tymethod.poll
//! [`Pol::Ready`]: https://doc.rust-lang.org/std/task/enum.Poll.html#variant.Ready
//! [`Poll::Pending`]: https://doc.rust-lang.org/std/task/enum.Poll.html#variant.Pending
//! [`Context`]: https://doc.rust-lang.org/std/task/struct.Context.html
//! [`Waker`]: https://doc.rust-lang.org/std/task/struct.Waker.html
//! [`RawWaker`]: https://doc.rust-lang.org/std/task/struct.RawWaker.html

use std::{
    future::Future,
    marker::PhantomData,
    ops::Deref,
    panic,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll, Wake},
};

use super::{RustFutureContinuationCallback, RustFuturePoll, Scheduler};
use crate::{rust_call_with_out_status, FfiDefault, LiftArgsError, LowerReturn, RustCallStatus};

/// Wraps the actual future we're polling
struct WrappedFuture<F, T, UT>
where
    // See rust_future_new for an explanation of these trait bounds
    F: Future<Output = Result<T, LiftArgsError>> + Send + 'static,
    T: LowerReturn<UT> + Send + 'static,
    UT: Send + 'static,
{
    // Note: this could be a single enum, but that would make it easy to mess up the future pinning
    // guarantee.   For example you might want to call `std::mem::take()` to try to get the result,
    // but if the future happened to be stored that would move and break all internal references.
    future: Option<F>,
    result: Option<Result<T::ReturnType, RustCallStatus>>,
}

impl<F, T, UT> WrappedFuture<F, T, UT>
where
    // See rust_future_new for an explanation of these trait bounds
    F: Future<Output = Result<T, LiftArgsError>> + Send + 'static,
    T: LowerReturn<UT> + Send + 'static,
    UT: Send + 'static,
{
    fn new(future: F) -> Self {
        Self {
            future: Some(future),
            result: None,
        }
    }

    // Poll the future and check if it's ready or not
    fn poll(&mut self, context: &mut Context<'_>) -> bool {
        if self.result.is_some() {
            true
        } else if let Some(future) = &mut self.future {
            // SAFETY: We can call Pin::new_unchecked because:
            //    - This is the only time we get a &mut to `self.future`
            //    - We never poll the future after it's moved (for example by using take())
            //    - We never move RustFuture, which contains us.
            //    - RustFuture is private to this module so no other code can move it.
            let pinned = unsafe { Pin::new_unchecked(future) };
            // Run the poll and lift the result if it's ready
            let mut out_status = RustCallStatus::default();
            let result: Option<Poll<T::ReturnType>> = rust_call_with_out_status(
                &mut out_status,
                // This closure uses a `&mut F` value, which means it's not UnwindSafe by
                // default.  If the future panics, it may be in an invalid state.
                //
                // However, we can safely use `AssertUnwindSafe` since a panic will lead the `None`
                // case below and we will never poll the future again.
                panic::AssertUnwindSafe(|| match pinned.poll(context) {
                    Poll::Pending => Ok(Poll::Pending),
                    Poll::Ready(Ok(v)) => T::lower_return(v).map(Poll::Ready),
                    Poll::Ready(Err(e)) => T::handle_failed_lift(e).map(Poll::Ready),
                }),
            );
            match result {
                Some(Poll::Pending) => false,
                Some(Poll::Ready(v)) => {
                    self.future = None;
                    self.result = Some(Ok(v));
                    true
                }
                None => {
                    self.future = None;
                    self.result = Some(Err(out_status));
                    true
                }
            }
        } else {
            trace!("poll with neither future nor result set");
            true
        }
    }

    fn complete(&mut self, out_status: &mut RustCallStatus) -> T::ReturnType {
        let mut return_value = T::ReturnType::ffi_default();
        match self.result.take() {
            Some(Ok(v)) => return_value = v,
            Some(Err(call_status)) => *out_status = call_status,
            None => *out_status = RustCallStatus::cancelled(),
        }
        self.free();
        return_value
    }

    fn free(&mut self) {
        self.future = None;
        self.result = None;
    }
}

// If F and T are Send, then WrappedFuture is too
//
// Rust will not mark it Send by default when T::ReturnType is a raw pointer.  This is promising
// that we will treat the raw pointer properly, for example by not returning it twice.
unsafe impl<F, T, UT> Send for WrappedFuture<F, T, UT>
where
    // See rust_future_new for an explanation of these trait bounds
    F: Future<Output = Result<T, LiftArgsError>> + Send + 'static,
    T: LowerReturn<UT> + Send + 'static,
    UT: Send + 'static,
{
}

/// Future that the foreign code is awaiting
pub(super) struct RustFuture<F, T, UT>
where
    // See rust_future_new for an explanation of these trait bounds
    F: Future<Output = Result<T, LiftArgsError>> + Send + 'static,
    T: LowerReturn<UT> + Send + 'static,
    UT: Send + 'static,
{
    // This Mutex should never block if our code is working correctly, since there should not be
    // multiple threads calling [Self::poll] and/or [Self::complete] at the same time.
    future: Mutex<WrappedFuture<F, T, UT>>,
    scheduler: Mutex<Scheduler>,
    // UT is used as the generic parameter for [LowerReturn].
    // Let's model this with PhantomData as a function that inputs a UT value.
    _phantom: PhantomData<fn(UT) -> ()>,
}

impl<F, T, UT> RustFuture<F, T, UT>
where
    // See rust_future_new for an explanation of these trait bounds
    F: Future<Output = Result<T, LiftArgsError>> + Send + 'static,
    T: LowerReturn<UT> + Send + 'static,
    UT: Send + 'static,
{
    pub(super) fn new(future: F, _tag: UT) -> Arc<Self> {
        Arc::new(Self {
            future: Mutex::new(WrappedFuture::new(future)),
            scheduler: Mutex::new(Scheduler::new()),
            _phantom: PhantomData,
        })
    }

    pub(super) fn poll(self: Arc<Self>, callback: RustFutureContinuationCallback, data: u64) {
        let cancelled = self.is_cancelled();
        let ready = cancelled || {
            let mut locked = self.future.lock().unwrap();
            let waker: std::task::Waker = Arc::clone(&self).into();
            locked.poll(&mut Context::from_waker(&waker))
        };
        if ready {
            trace!("RustFuture::poll is ready (cancelled: {cancelled})");
            callback(data, RustFuturePoll::Ready)
        } else {
            self.scheduler.lock().unwrap().store(callback, data);
        }
    }

    pub(super) fn is_cancelled(&self) -> bool {
        self.scheduler.lock().unwrap().is_cancelled()
    }

    pub(super) fn wake(&self) {
        trace!("RustFuture::wake called");
        self.scheduler.lock().unwrap().wake();
    }

    pub(super) fn cancel(&self) {
        self.scheduler.lock().unwrap().cancel();
    }

    pub(super) fn complete(&self, call_status: &mut RustCallStatus) -> T::ReturnType {
        self.future.lock().unwrap().complete(call_status)
    }

    pub(super) fn free(self: Arc<Self>) {
        // Call cancel() to send any leftover data to the continuation callback
        self.scheduler.lock().unwrap().cancel();
        // Ensure we drop our inner future, releasing all held references
        self.future.lock().unwrap().free();
    }
}

impl<F, T, UT> Wake for RustFuture<F, T, UT>
where
    // See rust_future_new for an explanation of these trait bounds
    F: Future<Output = Result<T, LiftArgsError>> + Send + 'static,
    T: LowerReturn<UT> + Send + 'static,
    UT: Send + 'static,
{
    fn wake(self: Arc<Self>) {
        self.deref().wake()
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.deref().wake()
    }
}

/// RustFuture FFI trait.  This allows `Arc<RustFuture<F, T, UT>>` to be cast to
/// `Arc<dyn RustFutureFfi<T::ReturnType>>`, which is needed to implement the public FFI API.  In particular, this
/// allows you to use RustFuture functionality without knowing the concrete Future type, which is
/// unnamable.
///
/// This is parametrized on the ReturnType rather than the `T` directly, to reduce the number of
/// scaffolding functions we need to generate.  If it was parametrized on `T`, then we would need
/// to create a poll, cancel, complete, and free scaffolding function for each exported async
/// function.  That would add ~1kb binary size per exported function based on a quick estimate on a
/// x86-64 machine . By parametrizing on `T::ReturnType` we can instead monomorphize by hand and
/// only create those functions for each of the 13 possible FFI return types.
#[doc(hidden)]
pub trait RustFutureFfi<ReturnType>: Send + Sync {
    fn ffi_poll(self: Arc<Self>, callback: RustFutureContinuationCallback, data: u64);
    fn ffi_cancel(&self);
    fn ffi_complete(&self, call_status: &mut RustCallStatus) -> ReturnType;
    fn ffi_free(self: Arc<Self>);
}

impl<F, T, UT> RustFutureFfi<T::ReturnType> for RustFuture<F, T, UT>
where
    // See rust_future_new for an explanation of these trait bounds
    F: Future<Output = Result<T, LiftArgsError>> + Send + 'static,
    T: LowerReturn<UT> + Send + 'static,
    UT: Send + 'static,
{
    fn ffi_poll(self: Arc<Self>, callback: RustFutureContinuationCallback, data: u64) {
        self.poll(callback, data)
    }

    fn ffi_cancel(&self) {
        self.cancel()
    }

    fn ffi_complete(&self, call_status: &mut RustCallStatus) -> T::ReturnType {
        self.complete(call_status)
    }

    fn ffi_free(self: Arc<Self>) {
        self.free();
    }
}
