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

#![warn(clippy::pedantic)]
#![allow(clippy::match_wild_err_arm)]
#![allow(clippy::single_match_else)]

use std::{
    any::Any,
    future::Future,
    marker::PhantomData,
    mem,
    pin::Pin,
    sync::Arc,
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
        result: Mutex::new(None),
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
    Receiver {
        inner: Arc::new(Inner {
            result: Mutex::new(Some(Box::new(result))),
            tow: Mutex::new(ThreadOrWaker::None),
        }),
        _marker: PhantomData,
    }
}

/* Sender and Receiver structs, for actually using the channel */

/// The sender for the two-way oneshot channel. It is consumed upon sending its data.
#[must_use]
pub struct Sender<I> {
    // the part of the heap where the object proper is kept
    inner: Arc<Inner>,
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
        self.inner.set_result(res);
        self.inner.wake();
    }
}

/// The receiver for the two-way oneshot channel. It is consumed upon receiving its data.
#[must_use]
pub struct Receiver<R> {
    // the part of the heap where the channel is kept
    inner: Arc<Inner>,
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
        match Box::<dyn Any + Send>::downcast::<R>(res) {
            Ok(res) => *res,
            Err(_) => panic!("Unable to cast oneshot result to desired value"),
        }
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
        match self.inner.get_result() {
            Some(res) => match Box::<dyn Any + Send>::downcast::<R>(res) {
                Ok(res) => Poll::Ready(*res),
                Err(_) => panic!("Unable to cast oneshot result to desired value"),
            },
            None => {
                *self.inner.tow.lock() = ThreadOrWaker::Waker(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

/// The inner state of the Two.
struct Inner {
    // the parker or waker we're blocked on
    tow: Mutex<ThreadOrWaker>,
    // the result to be delivered to the user
    result: Mutex<Option<Box<dyn Any + Send>>>,
}

impl Inner {
    #[inline]
    fn wake(&self) {
        let mut lock = self.tow.lock();
        match mem::take(&mut *lock) {
            ThreadOrWaker::None => (),
            ThreadOrWaker::Thread(t) => t.unpark(),
            ThreadOrWaker::Waker(w) => w.wake(),
        }
    }

    #[inline]
    fn get_result(&self) -> Option<Box<dyn Any + Send>> {
        let mut lock = self.result.lock();
        lock.take()
    }

    #[inline]
    fn set_result<T: Any + Send>(&self, result: T) {
        let mut lock = self.result.lock();
        *lock = Some(Box::new(result));
    }

    #[inline]
    fn park_until_result(&self) -> Box<dyn Any + Send> {
        loop {
            if let Some(res) = self.get_result() {
                return res;
            }

            let cur_thread = thread::current();
            *self.tow.lock() = ThreadOrWaker::Thread(cur_thread);

            if let Some(res) = self.get_result() {
                return res;
            }

            thread::park();
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
