use std::cmp;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::isize;
use std::iter::FusedIterator;
use std::mem;
use std::panic::{RefUnwindSafe, UnwindSafe};
use std::process;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use flavors;
use internal::select::CaseId;
use internal::select::Select;
use internal::select::Token;

// TODO: explain
// loop { try; register; is_blocked; unregister; accept; write/read }

// TODO: after and tick should be bounded(1) channels
// TODO: ticker start measuring time only when the channel is empty (document this!)
// TODO: after/ticker must send the instant at which the event was fired, not when the message was received! (test this)
// TODO: add a note saying that ticker/after don't spawn a thread/goroutine, i.e. they're cheap

// TODO: document what after is equivalent to in terms of thread::spawn code
// TODO: document the same for tick
// TODO: document that after never closes the channel (it's like mem::forgetting the sender)
#[inline]
pub fn after(dur: Duration) -> Receiver<Instant> {
    Receiver::After(flavors::after::Channel::new(dur))
}

#[inline]
pub fn tick(dur: Duration) -> Receiver<Instant> {
    Receiver::Tick(flavors::tick::Channel::new(dur))
}

pub struct Channel<T> {
    senders: AtomicUsize,
    flavor: Flavor<T>,
}

enum Flavor<T> {
    Array(flavors::array::Channel<T>),
    List(flavors::list::Channel<T>),
    Zero(flavors::zero::Channel<T>),
}

/// Creates a new channel of unbounded capacity, returning the sender and receiver halves.
///
/// This type of channel can hold any number of messages; i.e. it has infinite capacity.
///
/// # Examples
///
/// ```
/// use crossbeam_channel as channel;
///
/// use std::thread;
///
/// let (s, r) = channel::unbounded();
///
/// // An expensive computation.
/// fn fib(n: i32) -> i32 {
///     if n <= 1 {
///         n
///     } else {
///         fib(n - 1) + fib(n - 2)
///     }
/// }
///
/// // Spawn a thread doing an expensive computation.
/// thread::spawn(move || {
///     s.send(fib(20));
/// });
///
/// // Let's see what's the result of the computation.
/// println!("{}", r.recv().unwrap());
/// ```
pub fn unbounded<T>() -> (Sender<T>, Receiver<T>) {
    let chan = Arc::new(Channel {
        senders: AtomicUsize::new(0),
        flavor: Flavor::List(flavors::list::Channel::new()),
    });

    let s = Sender::new(chan.clone());
    let r = Receiver::new(chan);
    (s, r)
}

/// Creates a new channel of bounded capacity, returning the sender and receiver halves.
///
/// This type of channel has an internal buffer of length `cap` in which messages get queued.
///
/// An rather special case is zero-capacity channel, also known as *rendezvous* channel. Such a
/// channel cannot hold any messages since its buffer is of length zero. Instead, send and receive
/// operations must be executing at the same time in order to pair up and pass the message over.
///
/// # Examples
///
/// ```
/// use std::thread;
/// use std::time::Duration;
///
/// use crossbeam_channel as channel;
///
/// let (s, r) = channel::bounded(1);
///
/// // This call returns immediately since there is enough space in the channel.
/// s.send(1);
///
/// thread::spawn(move || {
///     // This call blocks the current thread because the channel is full. It will be able to
///     // complete only after the first message is received.
///     s.send(2);
/// });
///
/// thread::sleep(Duration::from_secs(1));
/// assert_eq!(r.recv(), Some(1));
/// assert_eq!(r.recv(), Some(2));
/// ```
///
/// ```
/// use std::thread;
/// use std::time::Duration;
///
/// use crossbeam_channel as channel;
///
/// let (s, r) = channel::bounded(0);
///
/// thread::spawn(move || {
///     // This call blocks the current thread until a receive operation appears on the other side
///     // of the channel.
///     s.send(1);
/// });
///
/// thread::sleep(Duration::from_secs(1));
/// assert_eq!(r.recv(), Some(1));
/// ```
pub fn bounded<T>(cap: usize) -> (Sender<T>, Receiver<T>) {
    let chan = Arc::new(Channel {
        senders: AtomicUsize::new(0),
        flavor: {
            if cap == 0 {
                Flavor::Zero(flavors::zero::Channel::new())
            } else {
                Flavor::Array(flavors::array::Channel::with_capacity(cap))
            }
        },
    });

    let s = Sender::new(chan.clone());
    let r = Receiver::new(chan);
    (s, r)
}

/// The sending half of a channel.
///
/// Senders can be cloned and shared among multiple threads.
///
/// # Examples
///
/// ```
/// use std::thread;
///
/// use crossbeam_channel as channel;
///
/// let (s1, r) = channel::unbounded();
/// let s2 = s1.clone();
///
/// thread::spawn(move || s1.send(1));
/// thread::spawn(move || s2.send(2));
///
/// let msg1 = r.recv().unwrap();
/// let msg2 = r.recv().unwrap();
///
/// assert_eq!(msg1 + msg2, 3);
/// ```
pub struct Sender<T>(Arc<Channel<T>>);

unsafe impl<T: Send> Send for Sender<T> {}
unsafe impl<T: Send> Sync for Sender<T> {}

impl<T> Sender<T> {
    /// Creates a new sender handle for the channel and increments the sender count.
    fn new(chan: Arc<Channel<T>>) -> Self {
        let old_count = chan.senders.fetch_add(1, Ordering::SeqCst);

        // Cloning senders and calling `mem::forget` on the clones could potentially overflow the
        // counter. It's very difficult to recover sensibly from such degenerate scenarios so we
        // just abort the process when the count becomes very large.
        if old_count > isize::MAX as usize {
            process::abort();
        }

        Sender(chan)
    }

    /// Returns a unique identifier for the channel.
    fn channel_id(&self) -> usize {
        &*self.0 as *const Channel<T> as usize
    }

    /// Sends a message into the channel, blocking the current thread if the channel is full.
    ///
    /// If called on a zero-capacity channel, this method blocks the current thread until a receive
    /// operation appears on the other side of the channel.
    ///
    /// Note: `s.send(msg)` is equivalent to `select! { send(s, msg) => {} }`.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::thread;
    /// use std::time::Duration;
    ///
    /// use crossbeam_channel as channel;
    ///
    /// let (s, r) = channel::bounded(0);
    ///
    /// thread::spawn(move || s.send(1));
    ///
    /// assert_eq!(r.recv(), Some(1));
    /// ```
    pub fn send(&self, msg: T) {
        match &self.0.flavor {
            Flavor::Array(chan) => chan.send(msg),
            Flavor::List(chan) => chan.send(msg),
            Flavor::Zero(chan) => chan.send(msg),
        }
    }

    /// Returns `true` if the channel is empty.
    ///
    /// Note: zero-capacity channels are always empty.
    ///
    /// # Examples
    ///
    /// ```
    /// use crossbeam_channel as channel;
    ///
    /// let (s, r) = channel::unbounded();
    /// assert!(s.is_empty());
    ///
    /// s.send(0);
    /// assert!(!s.is_empty());
    /// ```
    pub fn is_empty(&self) -> bool {
        match &self.0.flavor {
            Flavor::Array(chan) => chan.is_empty(),
            Flavor::List(chan) => chan.is_empty(),
            Flavor::Zero(_) => true,
        }
    }

    /// Returns `true` if the channel is full.
    ///
    /// Note: zero-capacity channels are always full.
    ///
    /// # Examples
    ///
    /// ```
    /// use crossbeam_channel as channel;
    ///
    /// let (s, r) = channel::bounded(1);
    ///
    /// assert!(!s.is_full());
    /// s.send(0);
    /// assert!(s.is_full());
    /// ```
    pub fn is_full(&self) -> bool {
        match &self.0.flavor {
            Flavor::Array(chan) => chan.is_full(),
            Flavor::List(_) => false,
            Flavor::Zero(_) => true,
        }
    }

    /// Returns the number of messages in the channel.
    ///
    /// # Examples
    ///
    /// ```
    /// use crossbeam_channel as channel;
    ///
    /// let (s, r) = channel::unbounded();
    /// assert_eq!(s.len(), 0);
    ///
    /// s.send(1);
    /// s.send(2);
    /// assert_eq!(s.len(), 2);
    /// ```
    pub fn len(&self) -> usize {
        match &self.0.flavor {
            Flavor::Array(chan) => chan.len(),
            Flavor::List(chan) => chan.len(),
            Flavor::Zero(_) => 0,
        }
    }

    /// If the channel is bounded, returns its capacity.
    ///
    /// # Examples
    ///
    /// ```
    /// use crossbeam_channel as channel;
    ///
    /// let (s, _) = channel::unbounded::<i32>();
    /// assert_eq!(s.capacity(), None);
    ///
    /// let (s, _) = channel::bounded::<i32>(5);
    /// assert_eq!(s.capacity(), Some(5));
    ///
    /// let (s, _) = channel::bounded::<i32>(0);
    /// assert_eq!(s.capacity(), Some(0));
    /// ```
    pub fn capacity(&self) -> Option<usize> {
        match &self.0.flavor {
            Flavor::Array(chan) => Some(chan.capacity()),
            Flavor::List(_) => None,
            Flavor::Zero(_) => Some(0),
        }
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        if self.0.senders.fetch_sub(1, Ordering::SeqCst) == 1 {
            match &self.0.flavor {
                Flavor::Array(chan) => chan.close(),
                Flavor::List(chan) => chan.close(),
                Flavor::Zero(chan) => chan.close(),
            };
        }
    }
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        Sender::new(self.0.clone())
    }
}

impl<T> fmt::Debug for Sender<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Sender").finish()
    }
}

impl<T> Hash for Sender<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.channel_id().hash(state)
    }
}

impl<T> PartialEq for Sender<T> {
    fn eq(&self, other: &Sender<T>) -> bool {
        self.channel_id() == other.channel_id()
    }
}

impl<T> Eq for Sender<T> {}

impl<T> PartialEq<Receiver<T>> for Sender<T> {
    fn eq(&self, other: &Receiver<T>) -> bool {
        self.channel_id() == other.channel_id()
    }
}

impl<T> UnwindSafe for Sender<T> {}
impl<T> RefUnwindSafe for Sender<T> {}

/// The receiving half of a channel.
///
/// Receivers can be cloned and shared among multiple threads.
///
/// # Examples
///
/// ```
/// use std::thread;
/// use std::time::Duration;
/// use crossbeam_channel as channel;
///
/// let (s, r) = channel::unbounded();
///
/// thread::spawn(move || {
///     s.send("Hello world!");
///     thread::sleep(Duration::from_secs(2)); // Block the current thread for two seconds.
///     s.send("Delayed for 2 seconds");
/// });
///
/// println!("{}", r.recv().unwrap()); // Received immediately.
/// println!("Waiting...");
/// println!("{}", r.recv().unwrap()); // Received after 2 seconds.
/// ```
// TODO: wrap this into an InnerReceiver<T> so that this becomes a struct with private internals
pub enum Receiver<T> {
    Channel(Arc<Channel<T>>),
    After(flavors::after::Channel),
    Tick(flavors::tick::Channel),
}

unsafe impl<T: Send> Send for Receiver<T> {}
unsafe impl<T: Send> Sync for Receiver<T> {}

impl<T> Receiver<T> {
    /// Creates a new receiver handle for the channel.
    fn new(chan: Arc<Channel<T>>) -> Self {
        Receiver::Channel(chan)
    }

    /// Returns a unique identifier for the channel.
    fn channel_id(&self) -> usize {
        match self {
            Receiver::Channel(chan) => &**chan as *const Channel<T> as usize,
            Receiver::After(chan) => chan.channel_id(),
            Receiver::Tick(chan) => chan.channel_id(),
        }
    }

    /// Blocks the current thread until a message is received or the channel is closed.
    ///
    /// Returns the message if it was received or `None` if the channel is closed.
    ///
    /// If called on a zero-capacity channel, this method blocks the current thread until a send
    /// operation appears on the other side of the channel.
    ///
    /// Note: `r.recv()` is equivalent to `select! { recv(r, msg) => msg }`.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::thread;
    /// use std::time::Duration;
    /// use crossbeam_channel as channel;
    ///
    /// let (s, r) = channel::unbounded();
    ///
    /// thread::spawn(move || {
    ///     thread::sleep(Duration::from_secs(1));
    ///     s.send(5);
    ///     // `s` gets dropped, closing the channel.
    /// });
    ///
    /// assert_eq!(r.recv(), Some(5));
    /// assert_eq!(r.recv(), None);
    /// ```
    pub fn recv(&self) -> Option<T> {
        match self {
            Receiver::Channel(arc) => match &arc.flavor {
                Flavor::Array(chan) => chan.recv(),
                Flavor::List(chan) => chan.recv(),
                Flavor::Zero(chan) => chan.recv(),
            },
            Receiver::After(chan) => unsafe {
                mem::transmute_copy::<Option<Instant>, Option<T>>(&chan.recv())
            }
            Receiver::Tick(chan) => unsafe {
                mem::transmute_copy::<Option<Instant>, Option<T>>(&chan.recv())
            }
        }
    }

    /// Attempts to receive a message from the channel without blocking.
    ///
    /// If there is no message ready to be received or the channel is closed, returns `None`.
    ///
    /// Note: `r.try_recv()` is equivalent to `select! { recv(r, msg) => msg, default => None }`.
    ///
    /// # Examples
    ///
    /// ```
    /// use crossbeam_channel as channel;
    ///
    /// let (s, r) = channel::unbounded();
    /// assert_eq!(r.try_recv(), None);
    ///
    /// s.send(5);
    /// drop(s);
    ///
    /// assert_eq!(r.try_recv(), Some(5));
    /// assert_eq!(r.try_recv(), None);
    /// ```
    pub fn try_recv(&self) -> Option<T> {
        // TODO: specialize! do we need try_send for zero-capacity?
        select! {
            recv(self, msg) => msg,
            default => None,
        }
    }

    /// Returns `true` if the channel is empty.
    ///
    /// Note: zero-capacity channels are always empty.
    ///
    /// # Examples
    ///
    /// ```
    /// use crossbeam_channel as channel;
    ///
    /// let (s, r) = channel::unbounded();
    ///
    /// assert!(r.is_empty());
    /// s.send(0);
    /// assert!(!r.is_empty());
    /// ```
    pub fn is_empty(&self) -> bool {
        match self {
            Receiver::Channel(arc) => match &arc.flavor {
                Flavor::Array(chan) => chan.is_empty(),
                Flavor::List(chan) => chan.is_empty(),
                Flavor::Zero(_) => true,
            },
            Receiver::After(chan) => chan.is_empty(),
            Receiver::Tick(chan) => chan.is_empty(),
        }
    }

    /// Returns `true` if the channel is full.
    ///
    /// Note: zero-capacity channels are always full.
    ///
    /// # Examples
    ///
    /// ```
    /// use crossbeam_channel as channel;
    ///
    /// let (s, r) = channel::bounded(1);
    ///
    /// assert!(!r.is_full());
    /// s.send(0);
    /// assert!(r.is_full());
    /// ```
    pub fn is_full(&self) -> bool {
        match self {
            Receiver::Channel(arc) => match &arc.flavor {
                Flavor::Array(chan) => chan.is_full(),
                Flavor::List(_) => false,
                Flavor::Zero(_) => true,
            },
            Receiver::After(chan) => !chan.is_empty(),
            Receiver::Tick(chan) => !chan.is_empty(),
        }
    }

    /// Returns the number of messages in the channel.
    ///
    /// # Examples
    ///
    /// ```
    /// use crossbeam_channel as channel;
    ///
    /// let (s, r) = channel::unbounded();
    /// assert_eq!(r.len(), 0);
    ///
    /// s.send(1);
    /// s.send(2);
    /// assert_eq!(r.len(), 2);
    /// ```
    pub fn len(&self) -> usize {
        match self {
            Receiver::Channel(arc) => match &arc.flavor {
                Flavor::Array(chan) => chan.len(),
                Flavor::List(chan) => chan.len(),
                Flavor::Zero(_) => 0,
            },
            Receiver::After(chan) => chan.len(),
            Receiver::Tick(chan) => chan.len(),
        }
    }

    /// If the channel is bounded, returns its capacity.
    ///
    /// # Examples
    ///
    /// ```
    /// use crossbeam_channel as channel;
    ///
    /// let (_, r) = channel::unbounded::<i32>();
    /// assert_eq!(r.capacity(), None);
    ///
    /// let (_, r) = channel::bounded::<i32>(5);
    /// assert_eq!(r.capacity(), Some(5));
    ///
    /// let (_, r) = channel::bounded::<i32>(0);
    /// assert_eq!(r.capacity(), Some(0));
    /// ```
    pub fn capacity(&self) -> Option<usize> {
        match self {
            Receiver::Channel(arc) => match &arc.flavor {
                Flavor::Array(chan) => Some(chan.capacity()),
                Flavor::List(_) => None,
                Flavor::Zero(_) => Some(0),
            },
            Receiver::After(_) => Some(1),
            Receiver::Tick(_) => Some(1),
        }
    }
}

impl<T> Clone for Receiver<T> {
    fn clone(&self) -> Self {
        match self {
            Receiver::Channel(arc) => Receiver::new(arc.clone()),
            Receiver::After(chan) => Receiver::After(chan.clone()),
            Receiver::Tick(chan) => Receiver::Tick(chan.clone()),
        }
    }
}

impl<T> fmt::Debug for Receiver<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Receiver").finish()
    }
}

impl<T> Hash for Receiver<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.channel_id().hash(state)
    }
}

impl<T> PartialEq for Receiver<T> {
    fn eq(&self, other: &Receiver<T>) -> bool {
        self.channel_id() == other.channel_id()
    }
}

impl<T> Eq for Receiver<T> {}

impl<T> PartialEq<Sender<T>> for Receiver<T> {
    fn eq(&self, other: &Sender<T>) -> bool {
        self.channel_id() == other.channel_id()
    }
}

impl<T> Iterator for Receiver<T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        self.recv()
    }
}

impl<T> FusedIterator for Receiver<T> {}

impl<'a, T> IntoIterator for &'a Receiver<T> {
    type Item = T;
    type IntoIter = Receiver<T>;

    fn into_iter(self) -> Self::IntoIter {
        self.clone()
    }
}

impl<T> UnwindSafe for Receiver<T> {}
impl<T> RefUnwindSafe for Receiver<T> {}

impl<T> Select for Sender<T> {
    fn try(&self, token: &mut Token) -> bool {
        match &self.0.flavor {
            Flavor::Array(chan) => chan.sender().try(token),
            Flavor::List(chan) => chan.sender().try(token),
            Flavor::Zero(chan) => chan.sender().try(token),
        }
    }

    fn retry(&self, token: &mut Token) -> bool {
        match &self.0.flavor {
            Flavor::Array(chan) => chan.sender().retry(token),
            Flavor::List(chan) => chan.sender().retry(token),
            Flavor::Zero(chan) => chan.sender().retry(token),
        }
    }

    fn deadline(&self) -> Option<Instant> {
        None
    }

    fn register(&self, token: &mut Token, case_id: CaseId) -> bool {
        match &self.0.flavor {
            Flavor::Array(chan) => chan.sender().register(token, case_id),
            Flavor::List(chan) => chan.sender().register(token, case_id),
            Flavor::Zero(chan) => chan.sender().register(token, case_id),
        }
    }

    fn unregister(&self, case_id: CaseId) {
        match &self.0.flavor {
            Flavor::Array(chan) => chan.sender().unregister(case_id),
            Flavor::List(chan) => chan.sender().unregister(case_id),
            Flavor::Zero(chan) => chan.sender().unregister(case_id),
        }
    }

    fn accept(&self, token: &mut Token) -> bool {
        match &self.0.flavor {
            Flavor::Array(chan) => chan.sender().accept(token),
            Flavor::List(chan) => chan.sender().accept(token),
            Flavor::Zero(chan) => chan.sender().accept(token),
        }
    }
}

pub unsafe fn write<T>(s: &Sender<T>, token: &mut Token, msg: T) {
    match &s.0.flavor {
        Flavor::Array(chan) => chan.write(token, msg),
        Flavor::List(chan) => chan.write(token, msg),
        Flavor::Zero(chan) => chan.write(token, msg),
    }
}

impl<T> Select for Receiver<T> {
    fn try(&self, token: &mut Token) -> bool {
        match self {
            Receiver::Channel(arc) => match &arc.flavor {
                Flavor::Array(chan) => chan.receiver().try(token),
                Flavor::List(chan) => chan.receiver().try(token),
                Flavor::Zero(chan) => chan.receiver().try(token),
            },
            Receiver::After(chan) => chan.try(token),
            Receiver::Tick(chan) => chan.try(token),
        }
    }

    fn retry(&self, token: &mut Token) -> bool {
        match self {
            Receiver::Channel(arc) => match &arc.flavor {
                Flavor::Array(chan) => chan.receiver().retry(token),
                Flavor::List(chan) => chan.receiver().retry(token),
                Flavor::Zero(chan) => chan.receiver().retry(token),
            },
            Receiver::After(chan) => chan.retry(token),
            Receiver::Tick(chan) => chan.retry(token),
        }
    }

    fn deadline(&self) -> Option<Instant> {
        match self {
            Receiver::Channel(_) => None,
            Receiver::After(chan) => chan.deadline(),
            Receiver::Tick(chan) => chan.deadline(),
        }
    }

    fn register(&self, token: &mut Token, case_id: CaseId) -> bool {
        match self {
            Receiver::Channel(arc) => match &arc.flavor {
                Flavor::Array(chan) => chan.receiver().register(token, case_id),
                Flavor::List(chan) => chan.receiver().register(token, case_id),
                Flavor::Zero(chan) => chan.receiver().register(token, case_id),
            },
            Receiver::After(_) => true,
            Receiver::Tick(_) => true,
        }
    }

    fn unregister(&self, case_id: CaseId) {
        match self {
            Receiver::Channel(arc) => match &arc.flavor {
                Flavor::Array(chan) => chan.receiver().unregister(case_id),
                Flavor::List(chan) => chan.receiver().unregister(case_id),
                Flavor::Zero(chan) => chan.receiver().unregister(case_id),
            },
            Receiver::After(_) => {}
            Receiver::Tick(_) => {}
        }
    }

    fn accept(&self, token: &mut Token) -> bool {
        match self {
            Receiver::Channel(arc) => match &arc.flavor {
                Flavor::Array(chan) => chan.receiver().accept(token),
                Flavor::List(chan) => chan.receiver().accept(token),
                Flavor::Zero(chan) => chan.receiver().accept(token),
            },
            Receiver::After(chan) => chan.accept(token),
            Receiver::Tick(chan) => chan.accept(token),
        }
    }
}

pub unsafe fn read<T>(r: &Receiver<T>, token: &mut Token) -> Option<T> {
    match r {
        Receiver::Channel(arc) => match &arc.flavor {
            Flavor::Array(chan) => chan.read(token),
            Flavor::List(chan) => chan.read(token),
            Flavor::Zero(chan) => chan.read(token),
        },
        Receiver::After(chan) => {
            mem::transmute_copy::<Option<Instant>, Option<T>>(&chan.read(token))
        },
        Receiver::Tick(chan) => {
            mem::transmute_copy::<Option<Instant>, Option<T>>(&chan.read(token))
        },
    }
}
