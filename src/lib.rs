// MIT/Apache2 License

//! This crate provides an abstraction for outsourcing a piece of work to another thread. This is a common
//! pattern in not only async programs, but OS-specific programs where it is expected that a certain piece of
//! work is done on a certain thread. To accomplish this, the crate provides a `Two`, short for "Two Way
//! Oneshot" (not a recursive acronym). It is implemented as a sender and a receiver, similar to other channels
//! in the Rust ecosystem. The primary difference is that the sender comes bundled with a data type of your
//! choice, and that the sender and receiver are both consumed upon data transmission.
//!
//! # Example
//!
//! Creates a thread that processes the work of squaring a number.
//!
//! ```
//! use orphan_crippler::two;
//! use std::thread;
//!
//! let (mut sender, receiver) = two::<i32, i32>(5);
//!
//! thread::spawn(move || {
//!     let input = sender.input().unwrap();
//!     let result = input * input;
//!     sender.send::<i32>(result);
//! });
//!
//! assert_eq!(receiver.recv(), 25);
//! ```
//!
//! # Features
//!
//! If the `parking_lot` feature is enabled, this crate uses mutexes from the `parking_lot` crate internally,
//! rather than `std::sync` mutexes. This is recommended if you are using `parking_lot` mutexes elsewhere in your
//! application.

#![deprecated = "use crossbeam_channel instead"]
#![warn(clippy::pedantic)]
#![allow(clippy::match_wild_err_arm)]
#![allow(clippy::single_match_else)]
#![allow(unused_unsafe)]

use std::{
    any::{self, Any},
    cell::UnsafeCell,
    future::Future,
    marker::PhantomData,
    mem::{self, MaybeUninit},
    pin::Pin,
    ptr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    task::{Context, Poll, Waker},
    thread::{self, Thread},
};

#[cfg(not(feature = "parking_lot"))]
use std::sync;

/// Creates a sender and a receiver for the two-way oneshot (`two`) channel.
///
/// # Example
///
/// ```
/// use orphan_crippler::two;
///
/// let (sender, receiver) = two::<i32, i32>(16);
/// ```
#[inline]
pub fn two<I, R: Any + Send>(input: I) -> (Sender<I>, Receiver<R>) {
    let inner = Arc::new(Inner {
        result: UnsafeCell::new(MaybeUninit::uninit()),
        is_result_set: AtomicBool::new(false),
        tow: Mutex::new(ThreadOrWaker::None),
    });

    let sender = Sender {
        inner: inner.clone(),
        input: Some(input),
    };

    let receiver = Receiver {
        inner,
        _marker: PhantomData,
    };

    (sender, receiver)
}

/// Equivalent to [`complete`], but uses a boxed `R` to avoid a layer of indirection, if you already have
/// one of those.
#[inline]
pub fn complete_boxed<R: Any + Send>(boxed: Box<R>) -> Receiver<R> {
    Receiver {
        inner: Arc::new(Inner {
            result: UnsafeCell::new(MaybeUninit::new(boxed)),
            is_result_set: AtomicBool::new(true),
            tow: Mutex::new(ThreadOrWaker::None),
        }),
        _marker: PhantomData,
    }
}

/// Creates a receiver that automatically resolves.
///
/// # Example
///
/// ```
/// use orphan_crippler::complete;
///
/// let recv = complete::<i32>(6);
/// assert_eq!(recv.recv(), 6);
/// ```
#[inline]
pub fn complete<R: Any + Send>(result: R) -> Receiver<R> {
    complete_boxed(Box::new(result))
}

/* Sender and Receiver structs, for actually using the channel */

/// The sender for the two-way oneshot channel. It is consumed upon sending its data.
#[must_use]
pub struct Sender<I> {
    // the part of the heap where the object proper is kept
    inner: Arc<dyn InnerGeneric + Send + Sync + 'static>,
    // the object we're supposed to be delivering
    input: Option<I>,
}

impl<I> Sender<I> {
    /// Get the input for this two-way oneshot channel.
    ///
    /// # Example
    ///
    /// ```
    /// use orphan_crippler::two;
    ///
    /// let (mut sender, _) = two::<i32, ()>(43);
    /// assert_eq!(sender.input(), Some(43));
    /// ```
    #[inline]
    pub fn input(&mut self) -> Option<I> {
        self.input.take()
    }

    /// Send the result back down the channel to the receiver.
    ///
    /// # Example
    ///
    /// ```
    /// use orphan_crippler::two;
    ///
    /// let (sender, receiver) = two::<(), i32>(());
    /// sender.send::<i32>(37);
    /// assert_eq!(receiver.recv(), 37);
    /// ```
    #[inline]
    pub fn send<T: Any + Send>(self, res: T) {
        // SAFETY: called before wake(), so we the end user won't be informed that there's a value until
        //         we're ready
        unsafe {
            self.inner.set_result(Box::new(res));
        }
        self.inner.wake();
    }
}

/// The receiver for the two-way oneshot channel. It is consumed upon receiving its data.
#[must_use]
pub struct Receiver<R> {
    // the part of the heap where the channel is kept
    inner: Arc<Inner<R>>,
    _marker: PhantomData<Option<R>>,
}

impl<R: Any + Send> Receiver<R> {
    /// Wait until we get a result.
    ///
    /// # Panics
    ///
    /// Panics if the sender thread sends a type other than `R` into the channel.
    ///
    /// # Example
    ///
    /// ```
    /// use orphan_crippler::two;
    ///
    /// let (mut sender, receiver) = two::<i32, i32>(2);
    /// let result = sender.input().unwrap() + 3;
    /// sender.send(result);
    /// assert_eq!(receiver.recv(), result);
    /// ```
    #[inline]
    #[must_use]
    pub fn recv(self) -> R {
        let res = self.inner.park_until_result();
        *res
    }

    /// Alias for recv()
    #[inline]
    #[must_use]
    pub fn wait(self) -> R {
        self.recv()
    }
}

impl<R: Any + Send> Future for Receiver<R> {
    type Output = R;

    #[inline]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<R> {
        if self.inner.is_result_set() {
            // SAFETY: we know the result is set!
            Poll::Ready(*unsafe { self.inner.get_result() })
        } else {
            *self.inner.tow.lock() = ThreadOrWaker::Waker(cx.waker().clone());
            Poll::Pending
        }
    }
}

/// The inner state of the Two.
struct Inner<T> {
    // the parker or waker we're blocked on
    tow: Mutex<ThreadOrWaker>,
    // whether or not "result" is set
    is_result_set: AtomicBool,
    // the result to be delivered to the user
    result: UnsafeCell<MaybeUninit<Box<T>>>,
}

// SAFETY: result is always protected by tow mutex and is_result_set
unsafe impl<T: Send> Send for Inner<T> {}
unsafe impl<T: Send> Sync for Inner<T> {}

impl<T: Any + Send> Inner<T> {
    #[inline]
    fn is_result_set(&self) -> bool {
        self.is_result_set.load(Ordering::Acquire)
    }

    #[inline]
    unsafe fn get_result(&self) -> Box<T> {
        // SAFETY: only ever called when there is only one reference left to the
        //         Inner<R>, or when we know the other reference is currently
        //         waiting on "tow" or "is_result_set"
        unsafe { MaybeUninit::assume_init(ptr::read(self.result.get())) }
    }

    #[inline]
    fn park_until_result(&self) -> Box<T> {
        loop {
            if self.is_result_set() {
                // SAFETY: we know the result is set
                return unsafe { self.get_result() };
            }

            let cur_thread = thread::current();
            *self.tow.lock() = ThreadOrWaker::Thread(cur_thread);

            if self.is_result_set() {
                // SAFETY: see above
                return unsafe { self.get_result() };
            }

            thread::park();
        }
    }
}

trait InnerGeneric {
    unsafe fn set_result(&self, item: Box<dyn Any + Send>);
    fn wake(&self);
}

impl<T: Any + Send> InnerGeneric for Inner<T> {
    #[inline]
    unsafe fn set_result(&self, item: Box<dyn Any + Send>) {
        // first, check to ensure we're using the proper type
        let item = match item.downcast::<T>() {
            Err(_) => panic!(
                "Passed item is not of expected type \"{}\"",
                any::type_name::<T>()
            ),
            Ok(item) => item,
        };

        // SAFETY: only called when is_result_set has yet to be set
        unsafe { ptr::write(self.result.get(), MaybeUninit::new(item)) };
        self.is_result_set.store(true, Ordering::Release);
    }

    #[inline]
    fn wake(&self) {
        let mut lock = self.tow.lock();
        match mem::take(&mut *lock) {
            ThreadOrWaker::None => (),
            ThreadOrWaker::Thread(t) => t.unpark(),
            ThreadOrWaker::Waker(w) => w.wake(),
        }
    }
}

enum ThreadOrWaker {
    None,
    Thread(Thread),
    Waker(Waker),
}

impl Default for ThreadOrWaker {
    #[inline]
    fn default() -> Self {
        Self::None
    }
}

#[cfg(feature = "parking_lot")]
#[repr(transparent)]
struct Mutex<T>(parking_lot::Mutex<T>);

#[cfg(feature = "parking_lot")]
impl<T> Mutex<T> {
    #[inline]
    fn new(val: T) -> Self {
        Self(parking_lot::Mutex::new(val))
    }

    #[inline]
    fn lock(&self) -> parking_lot::MutexGuard<'_, T> {
        self.0.lock()
    }
}

#[cfg(not(feature = "parking_lot"))]
struct Mutex<T>(sync::Mutex<T>);

#[cfg(not(feature = "parking_lot"))]
impl<T> Mutex<T> {
    #[inline]
    fn new(val: T) -> Self {
        Self(sync::Mutex::new(val))
    }

    #[inline]
    fn lock(&self) -> sync::MutexGuard<'_, T> {
        self.0.lock().expect("Unable to lock mutex")
    }
}
