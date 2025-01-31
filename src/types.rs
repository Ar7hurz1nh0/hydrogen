// Copyright 2015 Nathan Sizemore <nathanrsizemore@gmail.com>
//
// This Source Code Form is subject to the terms of the
// Mozilla Public License, v. 2.0. If a copy of the MPL was not
// distributed with this file, You can obtain one at
// http://mozilla.org/MPL/2.0/.


use std::io::{Error, ErrorKind};
use std::cell::UnsafeCell;
use std::sync::{Arc, Mutex};
use std::os::unix::io::{RawFd, AsRawFd};

use libc;
use simple_slab::Slab;

use super::{Stream, Handler};


/// Memory region for all concurrent connections.
pub type ConnectionSlab = Arc<MutSlab>;
/// Protected memory region for newly accepted connections.
pub type NewConnectionSlab = Arc<Mutex<Slab<Connection>>>;
/// Queue of Connections needing various I/O operations.
pub type IoQueue = Arc<Mutex<Vec<IoPair>>>;

#[derive(Clone, PartialEq, Eq)]
pub enum IoEvent {
    /// Epoll reported data is available on the socket for reading
    ReadAvailable,
    /// Epoll reported the socket is writable
    WriteAvailable,
    /// Epoll reported the socket is both writable and has data available for reading
    ReadWriteAvailable
}

#[derive(Clone)]
pub struct IoPair {
    /// The type of I/O needed on this Connection
    pub event: IoEvent,
    /// The connection `event` is paired with
    pub arc_connection: Arc<Connection>
}

pub struct Connection {
    /// Underlying file descriptor.
    pub fd: RawFd,
    /// A Some(Error) options means this connection is in
    /// an error'd state and should be closed.
    pub err_mutex: Mutex<Option<Error>>,
    /// Mutex to ensure thread safe, ordered writes to our streams.
    /// They may have internal buffers
    pub tx_mutex: Mutex<()>,
    /// Socket (Stream implemented trait-object).
    pub stream: Arc<UnsafeCell<dyn Stream>>
}
unsafe impl Send for Connection {}
unsafe impl Sync for Connection {}

pub struct MutSlab {
    pub inner: UnsafeCell<Slab<Arc<Connection>>>
}
unsafe impl Send for MutSlab {}
unsafe impl Sync for MutSlab {}

pub struct EventHandler(pub *mut dyn Handler);
unsafe impl Send for EventHandler {}
unsafe impl Sync for EventHandler {}
impl Clone for EventHandler {
    fn clone(&self) -> EventHandler {
        let EventHandler(ptr) = *self;
        unsafe {
            let same_location = &mut *ptr;
            EventHandler(same_location)
        }
    }
}

/// Thread-safe wrapper for consumer interaction with streams.
pub struct HydrogenSocket {
    /// The connection this socket represents
    pub arc_connection: Arc<Connection>,
    /// Function responsible for re-arming fd in epoll instance
    rearm_fn: unsafe fn(&Arc<Connection>, i32)
}

impl Clone for HydrogenSocket {
    fn clone(&self) -> HydrogenSocket {
        let fn_ptr = self.rearm_fn;
        HydrogenSocket {
            arc_connection: self.arc_connection.clone(),
            rearm_fn: fn_ptr
        }
    }
}

impl HydrogenSocket {
    pub fn new(arc_connection: Arc<Connection>,
               rearm_fn: unsafe fn(&Arc<Connection>, i32))
               -> HydrogenSocket
    {
        HydrogenSocket {
            arc_connection: arc_connection,
            rearm_fn: rearm_fn
        }
    }

    pub fn send(&self, buf: &[u8]) {
        let err;
        { // Mutex lock
            drop(match self.arc_connection.tx_mutex.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner()
            });

            let stream_ptr = self.arc_connection.stream.get();
            let write_result = unsafe {
                (*stream_ptr).send(buf)
            };
            if write_result.is_ok() {
                trace!("HydrogenSocket.send OK");
                return;
            }

            err = write_result.unwrap_err();
        } // Mutex unlock

        match err.kind() {
            ErrorKind::WouldBlock => {
                trace!("HydrogenSocket.send received WouldBlock");

                let execute = self.rearm_fn;
                unsafe {
                    execute(&(self.arc_connection), libc::EPOLLOUT);
                }
            }
            _ => {
                trace!("HydrogenSocket.send received err");

                { // Mutex lock
                    let mut err_state = match self.arc_connection.err_mutex.lock() {
                        Ok(g) => g,
                        Err(p) => p.into_inner()
                    };
                    *err_state = Some(err);
                } // Mutex unlock
            }
        }
    }

    pub fn shutdown(&mut self) -> Result<(), Error> {
        let stream_ptr = self.arc_connection.stream.get();
        unsafe {
            (*stream_ptr).shutdown()
        }
    }
}

impl AsRawFd for HydrogenSocket {
    fn as_raw_fd(&self) -> RawFd {
        self.arc_connection.fd
    }
}
