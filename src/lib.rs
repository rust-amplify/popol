use std::collections::HashMap;
use std::io;
use std::io::prelude::*;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::time;

#[allow(non_camel_case_types)]
type nfds_t = libc::c_ulong;

pub use events::Events;

pub mod events {
    /// Events that can be waited for.
    pub type Events = libc::c_short;

    /// The associated file is available for read operations.
    pub const POLLIN: Events = libc::POLLIN;
    /// There is urgent data available for read operations.
    pub const POLLPRI: Events = libc::POLLPRI;
    /// The associated file is available for write operations.
    pub const POLLOUT: Events = libc::POLLOUT;
    /// Error condition happened on the associated file descriptor.
    /// `poll` will always wait for this event; it is not necessary to set it.
    pub const POLLERR: Events = libc::POLLERR;
    /// Hang up happened on the associated file descriptor.
    /// `poll` will always wait for this event; it is not necessary to set it.
    pub const POLLHUP: Events = libc::POLLHUP;
    /// The associated file is invalid.
    /// `poll` will always wait for this event; it is not necessary to set it.
    pub const POLLNVAL: Events = libc::POLLNVAL;
    /// The associated file is ready to be read.
    pub const READ: Events = libc::POLLIN | libc::POLLHUP | libc::POLLERR | libc::POLLPRI;
    /// The associated file is ready to be written.
    pub const WRITE: Events = libc::POLLOUT | libc::POLLERR;
}

#[derive(Debug)]
pub struct Event<'a> {
    /// The file is writable.
    pub writable: bool,
    /// The file is readable.
    pub readable: bool,
    /// An error has occured on the file.
    pub errored: bool,
    /// The underlying descriptor.
    pub descriptor: &'a mut Descriptor,
}

impl<'a> Event<'a> {
    pub fn stream<T: FromRawFd>(&self) -> T {
        unsafe { T::from_raw_fd(self.descriptor.fd) }
    }
}

#[repr(C, packed)]
#[derive(Debug, Copy, Clone)]
pub struct Descriptor {
    fd: RawFd,
    events: Events,
    revents: Events,
}

impl Descriptor {
    pub fn new(fd: RawFd, events: Events) -> Self {
        Self {
            fd,
            events,
            revents: 0,
        }
    }

    pub fn set(&mut self, events: Events) {
        self.events = events;
    }

    pub fn unset(&mut self, events: Events) {
        self.events = self.events & !events;
    }
}

pub struct Descriptors<K> {
    wakers: HashMap<usize, UnixStream>,
    index: Vec<K>,
    list: Vec<Descriptor>,
}

impl<K: Eq + Copy + Clone> Descriptors<K> {
    pub fn new() -> Self {
        let wakers = HashMap::new();

        Self {
            wakers,
            index: vec![],
            list: vec![],
        }
    }

    pub fn with_capacity(cap: usize) -> Self {
        let wakers = HashMap::new();

        Self {
            wakers,
            index: Vec::with_capacity(cap),
            list: Vec::with_capacity(cap),
        }
    }

    pub fn register(&mut self, key: K, fd: &impl AsRawFd, events: Events) {
        self.insert(key, Descriptor::new(fd.as_raw_fd(), events));
    }

    pub fn unregister(&mut self, key: K) {
        if let Some(ix) = self.index.iter().position(|k| k == &key) {
            self.index.swap_remove(ix);
            self.list.swap_remove(ix);
        }
    }

    pub fn set(&mut self, key: K, events: Events) -> bool {
        if let Some(ix) = self.index.iter().position(|k| k == &key) {
            self.list[ix].events = events;
            return true;
        }
        false
    }

    pub fn insert(&mut self, key: K, descriptor: Descriptor) {
        self.index.push(key);
        self.list.push(descriptor);
    }
}

pub enum Poll<'a, K> {
    Timeout,
    Ready(Box<dyn Iterator<Item = (K, Event<'a>)> + 'a>),
}

impl<'a, K: 'a> Poll<'a, K> {
    pub fn iter(self) -> Box<dyn Iterator<Item = (K, Event<'a>)> + 'a> {
        match self {
            Self::Ready(iter) => iter,
            Self::Timeout => Box::new(std::iter::empty()),
        }
    }

    pub fn is_empty(&self) -> bool {
        matches!(self, Self::Timeout)
    }
}

/// Wakers are used to wake up `poll`.
///
/// Unlike the `mio` crate, it's okay to drop wakers after they have been used.
///
pub struct Waker {
    writer: UnixStream,
}

impl Waker {
    /// Create a new `Waker`.
    ///
    /// # Examples
    ///
    /// Wake a `Poll` instance from another thread.
    ///
    /// ```
    /// fn main() -> Result<(), Box<dyn std::error::Error>> {
    ///     use std::thread;
    ///     use std::time::Duration;
    ///
    ///     use popol::{Descriptors, Waker};
    ///
    ///     const WAKER: &'static str = "waker";
    ///
    ///     let mut descriptors = Descriptors::new();
    ///     let mut waker = Waker::new(&mut descriptors, WAKER)?;
    ///
    ///     let handle = thread::spawn(move || {
    ///         thread::sleep(Duration::from_millis(360));
    ///
    ///         // Wake the queue on the other thread.
    ///         waker.wake().expect("waking shouldn't fail");
    ///     });
    ///
    ///     // Wait to be woken up by the other thread. Otherwise, time out.
    ///     let result = popol::wait(&mut descriptors, Duration::from_secs(1))?;
    ///
    ///     assert!(!result.is_empty(), "There should be at least one event selected");
    ///
    ///     let mut events = result.iter();
    ///     let (key, event) = events.next().unwrap();
    ///
    ///     assert!(key == WAKER, "The event is triggered by the waker");
    ///     assert!(event.readable, "The event is readable");
    ///     assert!(events.next().is_none(), "There was only one event");
    ///
    ///     handle.join().unwrap();
    ///
    ///     Ok(())
    /// }
    /// ```
    pub fn new<K: Eq + Clone + Copy>(
        descriptors: &mut Descriptors<K>,
        key: K,
    ) -> io::Result<Waker> {
        let (writer, reader) = UnixStream::pair()?;
        let fd = reader.as_raw_fd();

        reader.set_nonblocking(true)?;
        writer.set_nonblocking(true)?;

        descriptors.wakers.insert(descriptors.list.len(), reader);
        descriptors.insert(key, Descriptor::new(fd, events::READ));

        Ok(Waker { writer })
    }

    pub fn wake(&mut self) -> io::Result<()> {
        self.writer.write(&[0x1])?;

        Ok(())
    }

    /// Unblock the waker. This allows it to be re-awoken.
    ///
    /// TODO: Unclear under what conditions this `read` can fail.
    ///       Possibly if interrupted by a signal?
    /// TODO: Handle `ErrorKind::Interrupted`
    /// TODO: Handle zero-length read?
    /// TODO: Handle `ErrorKind::WouldBlock`
    fn snooze(reader: &mut UnixStream) -> io::Result<()> {
        let mut buf = [0u8; 1];
        let n = reader.read(&mut buf)?;

        assert_eq!(n, 1);

        Ok(())
    }
}

pub fn wait<'a, K: Eq + Clone + Copy>(
    fds: &'a mut Descriptors<K>,
    timeout: time::Duration,
) -> Result<Poll<'a, K>, io::Error> {
    let timeout = timeout.as_millis() as libc::c_int;

    let result = unsafe {
        libc::poll(
            fds.list.as_mut_ptr() as *mut libc::pollfd,
            fds.list.len() as nfds_t,
            timeout,
        )
    };

    if result == 0 {
        Ok(Poll::Timeout)
    } else if result > 0 {
        let wakers = &mut fds.wakers;

        Ok(Poll::Ready(Box::new(
            fds.index
                .iter()
                .zip(fds.list.iter_mut())
                .enumerate()
                .filter(|(_, (_, d))| d.revents != 0)
                .map(move |(i, (key, descriptor))| {
                    let revents = descriptor.revents;

                    if let Some(reader) = wakers.get_mut(&i) {
                        assert!(revents & events::READ != 0);
                        assert!(revents & events::POLLOUT == 0);

                        Waker::snooze(reader).unwrap();
                    }

                    (
                        key.clone(),
                        Event {
                            readable: revents & events::READ != 0,
                            writable: revents & events::WRITE != 0,
                            errored: revents & (events::POLLERR | events::POLLNVAL) != 0,
                            descriptor,
                        },
                    )
                }),
        )))
    } else {
        Err(io::Error::last_os_error())
    }
}