//! Oneshot spsc (single producer, single consumer) channel. Meaning each channel instance
//! can only transport a single message. This has a few nice outcomes. One thing is that
//! the implementation can be very efficient, utilizing the knowledge that there will
//! only be one message. But more importantly, it allows the API to be expressed in such
//! a way that certain edge cases that you don't want to care about when only sending a
//! single message on a channel does not exist. For example: The sender can't be copied
//! or cloned, and the send method takes ownership and consumes the sender.
//! So you are guaranteed, at the type level, that there can only be one message sent.
//!
//! The sender's send method is non-blocking, and potentially lock- and wait-free.
//! See documentation on [Sender::send] for situations where it might not be fully wait-free.
//! The receiver supports both lock- and wait-free `try_recv` as well as indefinite and time
//! limited thread blocking receive operations. The receiver also implements `Future` and
//! supports asynchronously awaiting the message.
//!
//!
//! # Examples
//!
//! This example sets up a background worker that processes requests coming in on a standard
//! mpsc channel and replies on a oneshot channel provided with each request. The worker can
//! be interacted with both from sync and async contexts since the oneshot receiver
//! can receive both blocking and async.
//!
//! ```rust
//! # #[cfg(not(feature = "loom"))] {
//! use std::sync::mpsc;
//! use std::thread;
//! use std::time::Duration;
//!
//! type Request = String;
//!
//! // Starts a background thread performing some computation on requests sent to it.
//! // Delivers the response back over a oneshot channel.
//! fn spawn_processing_thread() -> mpsc::Sender<(Request, oneshot::Sender<usize>)> {
//!     let (request_sender, request_receiver) = mpsc::channel::<(Request, oneshot::Sender<usize>)>();
//!     thread::spawn(move || {
//!         for (request_data, response_sender) in request_receiver.iter() {
//!             let compute_operation = || request_data.len();
//!             let _ = response_sender.send(compute_operation()); // <- Send on the oneshot channel
//!         }
//!     });
//!     request_sender
//! }
//!
//! let processor = spawn_processing_thread();
//!
//! // If compiled with `std` the library can receive messages with timeout on regular threads
//! #[cfg(feature = "std")] {
//!     let (response_sender, response_receiver) = oneshot::channel();
//!     let request = Request::from("data from sync thread");
//!
//!     processor.send((request, response_sender)).expect("Processor down");
//!     match response_receiver.recv_timeout(Duration::from_secs(1)) { // <- Receive on the oneshot channel
//!         Ok(result) => println!("Processor returned {}", result),
//!         Err(oneshot::RecvTimeoutError::Timeout) => eprintln!("Processor was too slow"),
//!         Err(oneshot::RecvTimeoutError::Disconnected) => panic!("Processor exited"),
//!     }
//! }
//!
//! // If compiled with the `async` feature, the `Receiver` can be awaited in an async context
//! #[cfg(feature = "async")] {
//!     tokio::runtime::Runtime::new()
//!         .unwrap()
//!         .block_on(async move {
//!             let (response_sender, response_receiver) = oneshot::channel();
//!             let request = Request::from("data from sync thread");
//!
//!             processor.send((request, response_sender)).expect("Processor down");
//!             match response_receiver.await { // <- Receive on the oneshot channel asynchronously
//!                 Ok(result) => println!("Processor returned {}", result),
//!                 Err(_e) => panic!("Processor exited"),
//!             }
//!         });
//! }
//! # }
//! ```
//!
//! # Send has happens-before relationship with receive
//!
//! All the various ways the `Receiver` can obtain the message out of the channel is synchronized
//! with the `Sender`s `send` method. This means any operations and memory modifications done in
//! the sender thread before the call to `Sender::send` are guaranteed to happen before any code
//! running after the message has been received in the receiver thread.
//!
//! # Sync vs async
//!
//! The main motivation for writing this library was that there were no (known to me) channel
//! implementations allowing you to seamlessly send messages between a normal thread and an async
//! task, or the other way around. If message passing is the way you are communicating, of course
//! that should work smoothly between the sync and async parts of the program!
//!
//! This library achieves that by having a fast and cheap send operation that can
//! be used in both sync threads and async tasks. The receiver has both thread blocking
//! receive methods for synchronous usage, and implements `Future` for asynchronous usage.
//!
//! The receiving endpoint of this channel implements Rust's `Future` trait and can be waited on
//! in an asynchronous task. This implementation is completely executor/runtime agnostic. It should
//! be possible to use this library with any executor, or even pass messages between tasks running
//! in different executors.
//!

// # Implementation description
//
// When a channel is created via the `channel` function, it creates a single heap allocation
// containing:
// * A one byte atomic integer that represents the current channel state,
// * Uninitialized memory to fit the message,
// * Uninitialized memory to fit the waker that can wake the receiving task or thread up.
//
// The size of the waker depends on which features are activated, it ranges from 0 to 24 bytes[1].
// So with all features enabled each channel allocates 25 bytes plus the size of the
// message, plus any padding needed to get correct memory alignment.
//
// The Sender and Receiver only holds a raw pointer to the heap channel object. The last endpoint
// to be consumed or dropped is responsible for freeing the heap memory. The first endpoint to
// be consumed or dropped signal via the state that it is gone. And the second one see this and
// frees the memory.
//
// ## Footnotes
//
// [1]: Mind that the waker only takes zero bytes when all features are disabled, making it
//      impossible to *wait* for the message. `try_recv` is the only available method in this scenario.

#![deny(rust_2018_idioms)]
#![cfg_attr(not(feature = "std"), no_std)]
// Enables this nightly only feature for the documentation build on docs.rs.
// To test this locally, build the docs with:
// `RUSTDOCFLAGS="--cfg docsrs" cargo +nightly doc --all-features`
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(docsrs, feature(doc_auto_cfg))]

#[cfg(not(oneshot_loom))]
extern crate alloc;

use core::{mem, ptr::NonNull};

mod atomic;
use atomic::{Ordering::*, fence};

#[cfg(all(any(feature = "std", feature = "async"), not(oneshot_loom)))]
use core::hint;
#[cfg(all(any(feature = "std", feature = "async"), oneshot_loom))]
use loom::hint;

#[cfg(feature = "async")]
use core::{
    pin::Pin,
    task::{self, Poll},
};
#[cfg(feature = "std")]
use std::time::{Duration, Instant};

mod channel;
use channel::Channel;

mod sender;
pub use sender::Sender;

mod states;
use states::{DISCONNECTED, EMPTY, MESSAGE};
#[cfg(any(feature = "std", feature = "async"))]
use states::{RECEIVING, UNPARKING};

#[cfg(feature = "std")]
mod thread;

mod waker;

#[cfg(oneshot_loom)]
mod loombox;
#[cfg(not(oneshot_loom))]
use alloc::boxed::Box;
#[cfg(oneshot_loom)]
use loombox::Box;

mod errors;
// Wildcard imports are not nice. But since multiple errors have various conditional compilation,
// this is easier than doing three different imports.
pub use errors::*;

/// Creates a new oneshot channel and returns the two endpoints, [`Sender`] and [`Receiver`].
pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
    // Allocate the channel on the heap and get the pointer.
    // The last endpoint of the channel to be alive is responsible for freeing the channel
    // and dropping any object that might have been written to it.
    let channel_ptr = NonNull::from(Box::leak(Box::new(Channel::new())));

    (
        // SAFETY: The pointer is valid, and we only create a single `Sender`
        unsafe { Sender::new(channel_ptr) },
        Receiver { channel_ptr },
    )
}

/// Receiving end of a oneshot channel.
///
/// Created and returned from the [`channel`] function.
///
/// Can be used to receive a message from the corresponding [`Sender`]. How the message
/// can be received depends on what features are enabled.
///
/// This type implement [`IntoFuture`] when the `async` feature is enabled.
/// This allows awaiting it directly in an async context.
#[derive(Debug)]
pub struct Receiver<T> {
    // Covariance is the right choice here. Consider the example presented in Sender, and you'll
    // see that if we replaced `rx` instead then we would get the expected behavior
    channel_ptr: NonNull<Channel<T>>,
}

// SAFETY: The core functionality of this library is to be able to pass channel ends to different
// threads to then be able to pass messages between threads or tasks.
// The receiver only contains a pointer to the channel, and the entire library revolves around
// making sure the access to that channel object is properly synchronized
unsafe impl<T: Send> Send for Receiver<T> {}

// The Receiver can NOT be `Sync`! The current receive implementations that take `&self`
// assume no other receive operation runs in parallel.

impl<T> Unpin for Receiver<T> {}

impl<T> Receiver<T> {
    /// Checks if there is a message in the channel without blocking. Returns:
    ///  * `Ok(message)` if there was a message in the channel.
    ///  * `Err(Empty)` if the [`Sender`] is alive, but has not yet sent a message.
    ///  * `Err(Disconnected)` if the [`Sender`] was dropped before sending anything or if the
    ///    message has already been extracted by a previous receive call.
    ///
    /// If a message is returned, the channel is disconnected and any subsequent receive operation
    /// using this receiver will return an error.
    ///
    /// This method is completely lock-free and wait-free. The only thing it does is an atomic
    /// integer load of the channel state. And if there is a message in the channel it additionally
    /// performs one atomic integer store and copies the message from the heap to the stack for
    /// returning it.
    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        // SAFETY: The channel will not be freed while this method is still running.
        let channel = unsafe { self.channel_ptr.as_ref() };

        // ORDERING: Relaxed is fine since the only branch that needs synchronization is MESSAGE,
        // and that branch has its own synchronization.
        match channel.state().load(Relaxed) {
            MESSAGE => {
                // It's okay to break up the load and store since once we're in the message state
                // the sender no longer modifies the state
                // ORDERING: at this point the sender has done its job and is no longer active, so
                // we don't need to make any side effects visible to it
                channel.state().store(DISCONNECTED, Relaxed);

                // We need to establish a happens-before relationship with the sender's write
                // of the MESSAGE state in order to ensure that the message is visible to us.
                // This is a separate fence instead of part of the load above to optimize the
                // try_recv performance for the EMPTY case, since a common use case is in a polling
                // loop, and we only need to synchronize once the message has arrived.
                fence(Acquire);
                // SAFETY: The MESSAGE state + acquire ordering guarantees initialized message
                Ok(unsafe { channel.take_message() })
            }
            EMPTY => Err(TryRecvError::Empty),
            DISCONNECTED => Err(TryRecvError::Disconnected),
            #[cfg(feature = "async")]
            RECEIVING | UNPARKING => Err(TryRecvError::Empty),
            _ => unreachable!(),
        }
    }

    /// Attempts to wait for a message from the [`Sender`], returning an error if the channel is
    /// disconnected.
    ///
    /// This method will always block the current thread if there is no data available and it is
    /// still possible for the message to be sent. Once the message is sent to the corresponding
    /// [`Sender`], then this receiver will wake up and return that message.
    ///
    /// If the corresponding [`Sender`] has disconnected (been dropped), or it disconnects while
    /// this call is blocking, this call will wake up and return `Err` to indicate that the message
    /// can never be received on this channel.
    ///
    /// If a sent message has already been extracted from this channel this method will return an
    /// error.
    ///
    /// # Panics
    ///
    /// Panics if called after this receiver has been polled asynchronously.
    #[cfg(feature = "std")]
    pub fn recv(self) -> Result<T, RecvError> {
        // Note that we don't need to worry about changing the state to disconnected or setting the
        // state to an invalid value at any point in this function because we take ownership of
        // self, and this function does not exit until the message has been received or both ends
        // of the channel are inactive and cleaned up.

        let channel_ptr = self.channel_ptr;

        // Don't run our Drop implementation. This consuming recv method is responsible for freeing
        mem::forget(self);

        // SAFETY: the existence of the `self` parameter serves as a certificate that the receiver
        // is still alive, meaning that even if the sender was dropped then it would have observed
        // the fact that we're still alive and left the responsibility of deallocating the
        // channel to us, so channel_ptr is valid
        let channel = unsafe { channel_ptr.as_ref() };

        // ORDERING: We use relaxed ordering since all branches that need synchronization has
        // their own fences. They are standalone fences to optimize for the EMPTY case
        match channel.state().load(Relaxed) {
            // The sender is alive but has not sent anything yet. We prepare to park.
            EMPTY => {
                // Conditionally add a delay here to help the tests trigger the edge cases where
                // the sender manages to be dropped or send something before we are able to store
                // our waker object in the channel.
                #[cfg(all(oneshot_test_delay, not(oneshot_loom)))]
                std::thread::sleep(std::time::Duration::from_millis(10));

                // Write our waker instance to the channel.
                // SAFETY: we are not yet in the RECEIVING state, meaning that the sender will not
                // try to access the waker until it sees the state set to RECEIVING below
                unsafe { channel.write_waker(waker::ReceiverWaker::current_thread()) };

                // Switch the state to RECEIVING. We need to do this in one atomic step in case the
                // sender disconnected or sent the message while we wrote the waker to memory. We
                // don't need to do a compare exchange here however because if the original state
                // was not EMPTY, then the sender has either finished sending the message or is
                // being dropped, so the RECEIVING state will never be observed after we return.
                // ORDERING: we use release ordering so the sender can synchronize with our writing
                // of the waker to memory. The individual branches that need acquire ordering handle
                // it by themselves to not pay for it in the EMPTY case.
                match channel.state().swap(RECEIVING, Release) {
                    // We stored our waker, now we park until the sender has changed the state+
                    EMPTY => loop {
                        thread::park();

                        // ORDERING: Here we must acquire because we synchronize with two things the
                        // sender releases:
                        //  1. The write of the message.
                        //  2. The storing of the final state, so we know all code using the channel
                        //     has a happens-before relationship with our call to dealloc.
                        match channel.state().load(Acquire) {
                            // The sender sent the message while we were parked.
                            MESSAGE => {
                                // SAFETY: The MESSAGE state + acquire ordering guarantees
                                // initialized message
                                let message = unsafe { channel.take_message() };

                                // SAFETY: If the sender sent a message, it is our responsibility
                                // to deallocate the channel. The acquire load synchronize with the
                                // sender's final write of the MESSAGE state.
                                unsafe { dealloc(channel_ptr) };

                                break Ok(message);
                            }
                            // The sender was dropped while we were parked.
                            DISCONNECTED => {
                                // SAFETY: If the sender disconnected, it is our responsibility to
                                // deallocate the channel. The acquire load synchronize with the
                                // sender's final write of the DISCONNECTED state.
                                unsafe { dealloc(channel_ptr) };

                                break Err(RecvError);
                            }
                            // State did not change, spurious wakeup, park again.
                            RECEIVING | UNPARKING => (),
                            _ => unreachable!(),
                        }
                    },
                    // The sender sent the message while we prepared to park.
                    MESSAGE => {
                        // SAFETY: We wrote a waker above. The sender cannot have observed the
                        // RECEIVING state, so it has not accessed the waker. We must drop it.
                        unsafe { channel.drop_waker() };

                        // ORDERING: Synchronize with the write of the message and the sender's
                        // final write of the channel state. This branch is unlikely to be taken,
                        // so we use a dedicated fence instead putting it on the swap above.
                        fence(Acquire);

                        // SAFETY: The MESSAGE state + acquire ordering guarantees initialized
                        // message
                        let message = unsafe { channel.take_message() };

                        // SAFETY: If the sender wrote the message, that means we must free the
                        // channel. The acquire fence above ensures that the sender has completed
                        // its final write of the channel state.
                        unsafe { dealloc(channel_ptr) };

                        Ok(message)
                    }
                    // The sender was dropped before sending anything while we prepared to park.
                    DISCONNECTED => {
                        // SAFETY: We wrote a waker above. The sender cannot have observed the
                        // RECEIVING state, so it has not accessed the waker. We must drop it.
                        unsafe { channel.drop_waker() };

                        // ORDERING: Synchronize with the sender's final write of the channel
                        // state. This branch is unlikely to be taken, so we use a dedicated fence
                        // instead putting it on the swap above.
                        fence(Acquire);

                        // SAFETY: If the sender disconnected, that means we must free the channel.
                        // The acquire fence above ensures that the sender has completed its final
                        // write of the channel state.
                        unsafe { dealloc(channel_ptr) };

                        Err(RecvError)
                    }
                    _ => unreachable!(),
                }
            }
            // The sender already sent the message.
            MESSAGE => {
                // Establish happens-before relationship with the initialization of the message
                // and the sender's final write of the channel state.
                fence(Acquire);
                // SAFETY: we are in the MESSAGE state and have synchronized with the sender,
                // so the message is valid
                let message = unsafe { channel.take_message() };

                // SAFETY: we are already in the MESSAGE state so the sender has been forgotten
                // and it's our job to clean up resources
                unsafe { dealloc(channel_ptr) };

                Ok(message)
            }
            // The sender was dropped before sending anything, or we already received the message.
            DISCONNECTED => {
                // Establish happens-before relationship with the sender's final write of the
                // channel state.
                fence(Acquire);
                // SAFETY: the Sender delegates the responsibility of deallocating
                // the channel to us upon sending the message. We have synchronized with their
                // final write of the channel state.
                unsafe { dealloc(channel_ptr) };

                Err(RecvError)
            }
            // The receiver must have been `Future::poll`ed prior to this call.
            #[cfg(feature = "async")]
            RECEIVING | UNPARKING => panic!("{}", RECEIVER_USED_SYNC_AND_ASYNC_ERROR),
            _ => unreachable!(),
        }
    }

    /// Attempts to wait for a message from the [`Sender`], returning an error if the channel is
    /// disconnected. This is a non consuming version of [`Receiver::recv`], but with a bit
    /// worse performance. Prefer `[`Receiver::recv`]` if your code allows consuming the receiver.
    ///
    /// If a message is returned, the channel is disconnected and any subsequent receive operation
    /// using this receiver will return an error.
    ///
    /// # Panics
    ///
    /// Panics if called after this receiver has been polled asynchronously.
    #[cfg(feature = "std")]
    pub fn recv_ref(&self) -> Result<T, RecvError> {
        self.start_recv_ref(RecvError, |channel| {
            loop {
                thread::park();

                // ORDERING: we use acquire ordering to synchronize with the write of the message
                match channel.state().load(Acquire) {
                    // The sender sent the message while we were parked.
                    // We take the message and mark the channel disconnected.
                    MESSAGE => {
                        // ORDERING: the sender is inactive at this point so we don't need to make
                        // any reads or writes visible to the sending thread
                        channel.state().store(DISCONNECTED, Relaxed);

                        // SAFETY: The MESSAGE state + acquire ordering guarantees initialized
                        // message
                        break Ok(unsafe { channel.take_message() });
                    }
                    // The sender was dropped while we were parked.
                    DISCONNECTED => break Err(RecvError),
                    // State did not change, spurious wakeup, park again.
                    RECEIVING | UNPARKING => (),
                    _ => unreachable!(),
                }
            }
        })
    }

    /// Like [`Receiver::recv`], but will not block longer than `timeout`. Returns:
    ///  * `Ok(message)` if there was a message in the channel before the timeout was reached.
    ///  * `Err(Timeout)` if no message arrived on the channel before the timeout was reached.
    ///  * `Err(Disconnected)` if the sender was dropped before sending anything or if the message
    ///    has already been extracted by a previous receive call.
    ///
    /// If a message is returned, the channel is disconnected and any subsequent receive operation
    /// using this receiver will return an error.
    ///
    /// If the supplied `timeout` is so large that Rust's `Instant` type can't represent this point
    /// in the future this falls back to an indefinitely blocking receive operation.
    ///
    /// # Panics
    ///
    /// Panics if called after this receiver has been polled asynchronously.
    #[cfg(feature = "std")]
    pub fn recv_timeout(&self, timeout: Duration) -> Result<T, RecvTimeoutError> {
        match Instant::now().checked_add(timeout) {
            Some(deadline) => self.recv_deadline(deadline),
            None => self.recv_ref().map_err(|_| RecvTimeoutError::Disconnected),
        }
    }

    /// Like [`Receiver::recv`], but will not block longer than until `deadline`. Returns:
    ///  * `Ok(message)` if there was a message in the channel before the deadline was reached.
    ///  * `Err(Timeout)` if no message arrived on the channel before the deadline was reached.
    ///  * `Err(Disconnected)` if the sender was dropped before sending anything or if the message
    ///    has already been extracted by a previous receive call.
    ///
    /// If a message is returned, the channel is disconnected and any subsequent receive operation
    /// using this receiver will return an error.
    ///
    /// # Panics
    ///
    /// Panics if called after this receiver has been polled asynchronously.
    #[cfg(feature = "std")]
    pub fn recv_deadline(&self, deadline: Instant) -> Result<T, RecvTimeoutError> {
        /// # Safety
        ///
        /// If the sender is unparking us after a message send, the message must already have been
        /// written to the channel and an acquire memory barrier issued before calling this function
        #[cold]
        unsafe fn wait_for_unpark<T>(channel: &Channel<T>) -> Result<T, RecvTimeoutError> {
            loop {
                thread::park();

                // ORDERING: The callee has already synchronized with any message write
                match channel.state().load(Relaxed) {
                    MESSAGE => {
                        // ORDERING: the sender has been dropped, so this update only
                        // needs to be visible to us
                        channel.state().store(DISCONNECTED, Relaxed);
                        // SAFETY: The MESSAGE state + acquire ordering in callee
                        // guarantees initialized message
                        break Ok(unsafe { channel.take_message() });
                    }
                    DISCONNECTED => break Err(RecvTimeoutError::Disconnected),
                    // The sender is still unparking us. We continue on the empty state here since
                    // the current implementation eagerly sets the state to EMPTY upon timeout.
                    EMPTY => (),
                    _ => unreachable!(),
                }
            }
        }

        self.start_recv_ref(RecvTimeoutError::Disconnected, |channel| {
            loop {
                match deadline.checked_duration_since(Instant::now()) {
                    Some(timeout) => {
                        thread::park_timeout(timeout);

                        // ORDERING: synchronize with the write of the message
                        match channel.state().load(Acquire) {
                            // The sender sent the message while we were parked.
                            MESSAGE => {
                                // ORDERING: the sender has been `mem::forget`-ed so this update
                                // only needs to be visible to us.
                                channel.state().store(DISCONNECTED, Relaxed);

                                // SAFETY: The MESSAGE state + acquire ordering guarantees
                                // initialized message
                                break Ok(unsafe { channel.take_message() });
                            }
                            // The sender was dropped while we were parked.
                            DISCONNECTED => break Err(RecvTimeoutError::Disconnected),
                            // State did not change, spurious wakeup, park again.
                            RECEIVING | UNPARKING => (),
                            _ => unreachable!(),
                        }
                    }
                    None => {
                        // ORDERING: synchronize with the write of the message
                        match channel.state().swap(EMPTY, Acquire) {
                            // We reached the end of the timeout without receiving a message
                            RECEIVING => {
                                // SAFETY: we were in the receiving state and are now in the empty
                                // state, so the sender has not and will not try to read the waker,
                                // so we have exclusive access to drop it.
                                unsafe { channel.drop_waker() };

                                break Err(RecvTimeoutError::Timeout);
                            }
                            // The sender sent the message while we were parked.
                            MESSAGE => {
                                // Same safety and ordering as the Some branch

                                channel.state().store(DISCONNECTED, Relaxed);

                                // SAFETY: The MESSAGE state + acquire ordering guarantees
                                // initialized message
                                break Ok(unsafe { channel.take_message() });
                            }
                            // The sender was dropped while we were parked.
                            DISCONNECTED => {
                                // ORDERING: we were originally in the disconnected state meaning
                                // that the sender is inactive and no longer observing the state,
                                // so we only need to change it back to DISCONNECTED for if the
                                // receiver is dropped or a recv* method is called again
                                channel.state().store(DISCONNECTED, Relaxed);

                                break Err(RecvTimeoutError::Disconnected);
                            }
                            // The sender sent the message and started unparking us
                            UNPARKING => {
                                // We were in the UNPARKING state and are now in the EMPTY state.
                                // We wait to be properly unparked and to observe if the sender
                                // sets MESSAGE or DISCONNECTED state.
                                // SAFETY: The load above has synchronized with any message write.
                                break unsafe { wait_for_unpark(channel) };
                            }
                            _ => unreachable!(),
                        }
                    }
                }
            }
        })
    }

    /// Returns true if the associated [`Sender`] was dropped before sending a message. Or if
    /// the message has already been received.
    ///
    /// If `true` is returned, all future calls to receive methods are guaranteed to return
    /// a disconnected error. And future calls to this method is guaranteed to also return `true`.
    pub fn is_closed(&self) -> bool {
        // SAFETY: the existence of the `self` parameter serves as a certificate that the receiver
        // is still alive, meaning that even if the sender was dropped then it would have observed
        // the fact that we're still alive and left the responsibility of deallocating the
        // channel to us, so `self.channel` is valid
        let channel = unsafe { self.channel_ptr.as_ref() };

        // ORDERING: We *chose* a Relaxed ordering here as it is sufficient to
        // enforce the method's contract. Once true has been observed, it will remain true.
        // However, if false is observed, the sender might have just disconnected but this thread
        // has not observed it yet.
        channel.state().load(Relaxed) == DISCONNECTED
    }

    /// Returns true if there is a message in the channel, ready to be received.
    ///
    /// If `true` is returned, the next call to a receive method is guaranteed to return
    /// a message.
    pub fn has_message(&self) -> bool {
        // SAFETY: the existence of the `self` parameter serves as a certificate that the receiver
        // is still alive, meaning that even if the sender was dropped then it would have observed
        // the fact that we're still alive and left the responsibility of deallocating the
        // channel to us, so `self.channel` is valid
        let channel = unsafe { self.channel_ptr.as_ref() };

        // ORDERING: An acquire ordering is used to guarantee no subsequent loads is reordered
        // before this one. This upholds the contract that if true is returned, the next call to
        // a receive method is guaranteed to also observe the `MESSAGE` state and return a message.
        channel.state().load(Acquire) == MESSAGE
    }

    /// Begins the process of receiving on the channel by reference. If the message is already
    /// ready, or the sender has disconnected, then this function will return the appropriate
    /// Result immediately. Otherwise, it will write the waker to memory, check to see if the
    /// sender has finished or disconnected again, and then will call `finish`. `finish` is
    /// thus responsible for cleaning up the channel's resources appropriately before it returns,
    /// such as destroying the waker, for instance.
    #[cfg(feature = "std")]
    #[inline]
    fn start_recv_ref<E>(
        &self,
        disconnected_error: E,
        finish: impl FnOnce(&Channel<T>) -> Result<T, E>,
    ) -> Result<T, E> {
        // SAFETY: the existence of the `self` parameter serves as a certificate that the receiver
        // is still alive, meaning that even if the sender was dropped then it would have observed
        // the fact that we're still alive and left the responsibility of deallocating the
        // channel to us, so `self.channel` is valid
        let channel = unsafe { self.channel_ptr.as_ref() };

        // ORDERING: synchronize with the write of the message
        match channel.state().load(Acquire) {
            // The sender is alive but has not sent anything yet. We prepare to park.
            EMPTY => {
                // Conditionally add a delay here to help the tests trigger the edge cases where
                // the sender manages to be dropped or send something before we are able to store
                // our waker object in the channel.
                #[cfg(all(oneshot_test_delay, not(oneshot_loom)))]
                std::thread::sleep(std::time::Duration::from_millis(10));

                // Write our waker instance to the channel.
                // SAFETY: we are not yet in the RECEIVING state, meaning that the sender will not
                // try to access the waker until it sees the state set to RECEIVING below
                unsafe { channel.write_waker(waker::ReceiverWaker::current_thread()) };

                // ORDERING: we use release ordering on success so the sender can synchronize with
                // our write of the waker. We use relaxed ordering on failure since the sender does
                // not need to synchronize with our write and the individual match arms handle any
                // additional synchronization
                match channel.state().swap(RECEIVING, Release) {
                    // We stored our waker, now we delegate to the callback to finish the receive
                    // operation
                    EMPTY => finish(channel),
                    // The sender sent the message while we prepared to finish
                    MESSAGE => {
                        // SAFETY: We wrote a waker above. The sender cannot have observed the
                        // RECEIVING state, so it has not accessed the waker. We must drop it.
                        unsafe { channel.drop_waker() };

                        // ORDERING: Can be relaxed since this only needs to be visible to us (drop)
                        channel.state().store(DISCONNECTED, Relaxed);

                        // ORDERING: Synchronize with the write of the message and the sender's
                        // final write of the channel state. This branch is unlikely to be taken,
                        // so we use a dedicated fence instead putting it on the swap above.
                        fence(Acquire);

                        // SAFETY: The MESSAGE state + acquire ordering guarantees initialized
                        // message
                        Ok(unsafe { channel.take_message() })
                    }
                    // The sender was dropped before sending anything while we prepared to park.
                    DISCONNECTED => {
                        // SAFETY: We wrote a waker above. The sender cannot have observed the
                        // RECEIVING state, so it has not accessed the waker. We must drop it.
                        unsafe { channel.drop_waker() };

                        // ORDERING: Can be relaxed since this only needs to be visible to us (drop)
                        channel.state().store(DISCONNECTED, Relaxed);

                        // ORDERING: Synchronize with the sender's final write of the channel
                        // state. This branch is unlikely to be taken, so we use a dedicated fence
                        // instead putting it on the swap above.
                        fence(Acquire);

                        Err(disconnected_error)
                    }
                    _ => unreachable!(),
                }
            }
            // The sender sent the message. We take the message and mark the channel disconnected.
            MESSAGE => {
                // ORDERING: the sender has been `mem::forget`-ed so this update only needs to be
                // visible to us
                channel.state().store(DISCONNECTED, Relaxed);

                // SAFETY: we are in the message state so the message is valid
                Ok(unsafe { channel.take_message() })
            }
            // The sender was dropped before sending anything, or we already received the message.
            DISCONNECTED => Err(disconnected_error),
            // The receiver must have been `Future::poll`ed prior to this call.
            #[cfg(feature = "async")]
            RECEIVING | UNPARKING => panic!("{}", RECEIVER_USED_SYNC_AND_ASYNC_ERROR),
            _ => unreachable!(),
        }
    }

    /// Consumes the Receiver, returning a raw pointer to the channel on the heap.
    ///
    /// This is intended to simplify using oneshot channels with some FFI code. The only safe thing
    /// to do with the returned pointer is to later reconstruct the Receiver with
    /// [Receiver::from_raw]. Memory will leak if the Receiver is never reconstructed.
    pub fn into_raw(self) -> *mut () {
        let raw = self.channel_ptr.as_ptr() as *mut ();
        mem::forget(self);
        raw
    }

    /// Consumes a raw pointer from [Receiver::into_raw], recreating the Receiver.
    ///
    /// # Safety
    ///
    /// This pointer must have come from [`Receiver<T>::into_raw`] with the same message type, `T`.
    /// At most one Receiver must exist for a channel at any point in time.
    /// Constructing multiple Receivers from the same raw pointer leads to undefined behavior.
    pub unsafe fn from_raw(raw: *mut ()) -> Self {
        // SAFETY: Method guarantee that the pointer is valid and points to a Channel<T>.
        let channel_ptr = unsafe { NonNull::new_unchecked(raw as *mut Channel<T>) };
        Self { channel_ptr }
    }
}

#[cfg(feature = "async")]
impl<T> core::future::Future for Receiver<T> {
    type Output = Result<T, RecvError>;

    fn poll(self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Self::Output> {
        // SAFETY: the existence of the `self` parameter serves as a certificate that the receiver
        // is still alive, meaning that even if the sender was dropped then it would have observed
        // the fact that we're still alive and left the responsibility of deallocating the
        // channel to us, so `self.channel` is valid
        let channel = unsafe { self.channel_ptr.as_ref() };

        // ORDERING: We use relaxed ordering since all branches that need synchronization has
        // their own fences. They are standalone fences to optimize for the EMPTY case
        match channel.state().load(Relaxed) {
            // The sender is alive but has not sent anything yet.
            EMPTY => {
                // SAFETY: We can't be in the forbidden states, and no waker in the channel.
                unsafe { channel.write_async_waker(cx) }
            }
            // We were polled again while waiting for the sender. Replace the waker with the new one.
            RECEIVING => {
                // ORDERING: We use relaxed ordering on both success and failure since we have not
                // written anything above that must be released, and the individual match arms
                // handle any additional synchronization.
                match channel
                    .state()
                    .compare_exchange(RECEIVING, EMPTY, Relaxed, Relaxed)
                {
                    // We successfully changed the state back to EMPTY. Replace the waker.
                    // This is the most likely branch to be taken, which is why we don't use any
                    // memory barriers in the compare_exchange above.
                    Ok(_) => {
                        // SAFETY: We wrote the waker in a previous call to poll. We do not need
                        // a memory barrier since the previous write here was by ourselves.
                        unsafe { channel.drop_waker() };
                        // SAFETY: We can't be in the forbidden states, and no waker in the channel.
                        unsafe { channel.write_async_waker(cx) }
                    }
                    // The sender sent the message while we prepared to replace the waker.
                    // We take the message and mark the channel disconnected.
                    // The sender has already taken the waker.
                    Err(MESSAGE) => {
                        // ORDERING: the sender has been dropped so this update only needs to be
                        // visible to us
                        channel.state().store(DISCONNECTED, Relaxed);

                        // ORDERING: Synchronize with the write of the message.
                        fence(Acquire);

                        // SAFETY: The MESSAGE state + acquire ordering guarantees initialized
                        // message
                        Poll::Ready(Ok(unsafe { channel.take_message() }))
                    }
                    // The sender was dropped before sending anything while we prepared to park.
                    // The sender has taken the waker already.
                    Err(DISCONNECTED) => Poll::Ready(Err(RecvError)),
                    // The sender is currently waking us up.
                    Err(UNPARKING) => {
                        // We can't trust that the old waker that the sender has access to
                        // is honored by the async runtime at this point. So we wake ourselves
                        // up to get polled instantly again.
                        cx.waker().wake_by_ref();
                        Poll::Pending
                    }
                    _ => unreachable!(),
                }
            }
            // The sender sent the message.
            MESSAGE => {
                // ORDERING: the sender has been dropped so this update only needs to be
                // visible to us
                channel.state().store(DISCONNECTED, Relaxed);

                // ORDERING: Synchronize with the write of the message and the sender's
                // final write of the channel state. This branch is unlikely to be taken,
                // so we use a dedicated fence instead putting it on the swap above.
                fence(Acquire);

                // SAFETY: The MESSAGE state + acquire ordering guarantees initialized message
                Poll::Ready(Ok(unsafe { channel.take_message() }))
            }
            // The sender was dropped before sending anything, or we already received the message.
            DISCONNECTED => Poll::Ready(Err(RecvError)),
            // The sender has observed the RECEIVING state and is currently reading the waker from
            // a previous poll. We need to loop here until we observe the MESSAGE or DISCONNECTED
            // state. We busy loop here since we know the sender is done very soon.
            UNPARKING => loop {
                hint::spin_loop();
                // ORDERING: The load above has already synchronized with the write of the message.
                match channel.state().load(Relaxed) {
                    MESSAGE => {
                        // ORDERING: the sender has been dropped, so this update only
                        // needs to be visible to us
                        channel.state().store(DISCONNECTED, Relaxed);

                        // ORDERING: Synchronize with the write of the message and the sender's
                        // final write of the channel state. This branch is unlikely to be taken,
                        // so we use a dedicated fence instead putting it on the swap above.
                        fence(Acquire);

                        // SAFETY: The MESSAGE state + acquire ordering guarantees initialized
                        // message
                        break Poll::Ready(Ok(unsafe { channel.take_message() }));
                    }
                    // The sender was dropped. Our drop impl will synchronize with the sender's
                    // final write of the channel state and deallocate the channel.
                    DISCONNECTED => break Poll::Ready(Err(RecvError)),
                    // Sender is still unparking us... Spin more
                    UNPARKING => (),
                    _ => unreachable!(),
                }
            },
            _ => unreachable!(),
        }
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        // SAFETY: since the receiving side is still alive it should not have written any state
        // that signal to the sender to deallocate the channel. So the channel pointer is valid
        let channel = unsafe { self.channel_ptr.as_ref() };

        // If this receiver was previously polled, but was not polled to completion, then the
        // channel is in the RECEIVING state and with a waker written. We must tell the sender
        // we are no longer receiving, and then drop the waker. We must first move away from
        // the RECEIVING state in order to not race with the sender around taking the waker.
        #[cfg(feature = "async")]
        if channel.state().load(Relaxed) == RECEIVING
            && channel
                .state()
                .compare_exchange(RECEIVING, EMPTY, Relaxed, Relaxed)
                .is_ok()
        {
            // SAFETY: The RECEIVING state guarantees we have written a waker.
            unsafe { channel.drop_waker() };
        }

        // Set the channel state to disconnected and read what state the channel was in
        // ORDERING: Release is required so that in the states where the sender becomes responsible
        // for deallocating the channel, they can synchronize with this final state swap store from
        // us. Acquire is required by most branches to synchronize with writes in the sender.
        // See each branch for details.
        match channel.state().swap(DISCONNECTED, AcqRel) {
            // The sender has not sent anything, nor is it dropped. The sender is responsible for
            // deallocating the channel.
            EMPTY => (),
            // The sender already sent something. We must drop the message, and free the channel.
            MESSAGE => {
                // SAFETY: The MESSAGE state plus acquire ordering guarantees the sender has
                // written a message and that it has a happens-before relationship with this drop.
                unsafe { channel.drop_message() };

                // SAFETY: The acquire ordering of the swap above synchronize with the sender's
                // final write of the state. So we can safely deallocate it.
                unsafe { dealloc(self.channel_ptr) };
            }
            // The sender was already dropped. We are responsible for freeing the channel.
            DISCONNECTED => {
                // SAFETY: The acquire ordering of the swap above synchronize with the sender's
                // final write of the state. So we can safely deallocate it.
                unsafe { dealloc(self.channel_ptr) };
            }
            // This receiver was previously polled. The channel must have been in the RECEIVING
            // state. But the sender has observed the RECEIVING state and is currently reading the
            // waker to wake us up. We need to loop here until we observe the MESSAGE or
            // DISCONNECTED state. We busy loop here since we know the sender is done very soon.
            #[cfg(any(feature = "std", feature = "async"))]
            UNPARKING => {
                loop {
                    hint::spin_loop();
                    // ORDERING: The state swap above has already synchronized with the write of
                    // the message. Here we only need to observe the actual state change.
                    match channel.state().load(Relaxed) {
                        MESSAGE => {
                            // SAFETY: The message state plus acquire ordering from the state swap
                            // at the start of this drop guarantees a message has been written with
                            // happens-before synchronization
                            unsafe { channel.drop_message() };
                            break;
                        }
                        DISCONNECTED => break,
                        UNPARKING => (),
                        _ => unreachable!(),
                    }
                }
                // ORDERING: We need to synchronize with the sender's final write of the state.
                // This is a separate fence to allow the load in the busy loop above to use the
                // faster relaxed ordering.
                fence(Acquire);
                // SAFETY: Happens-before relationship with the sender's final write of the state
                // is established with the acquire fence above.
                unsafe { dealloc(self.channel_ptr) };
            }
            _ => unreachable!(),
        }
    }
}

#[cfg(all(feature = "std", feature = "async"))]
const RECEIVER_USED_SYNC_AND_ASYNC_ERROR: &str =
    "Invalid to call a blocking receive method on oneshot::Receiver after it has been polled";

/// Deallocates the channel's heap allocation (created in `oneshot::channel()`).
///
/// # Safety
///
/// * `channel` must be a valid pointer to a `Channel<T>` originally coming from
///   `oneshot::channel()`.
/// * The thread calling this function must have properly synchronized with any other thread
///   that has used the channel (either the `Sender` or `Receiver`). This means having an
///   acquire memory barrier on or after the loading of `channel.state` that determined that
///   the other thread is fully done using the channel and we are responsible for freeing it.
#[inline]
pub(crate) unsafe fn dealloc<T>(channel: NonNull<Channel<T>>) {
    // SAFETY: Method guarantee that the pointer is valid and points to a Channel<T>.
    drop(unsafe { Box::from_raw(channel.as_ptr()) });
}
