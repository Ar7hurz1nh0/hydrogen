// Copyright 2015 Nathan Sizemore <nathanrsizemore@gmail.com>
//
// This Source Code Form is subject to the terms of the
// Mozilla Public License, v. 2.0. If a copy of the MPL was not
// distributed with this file, You can obtain one at
// http://mozilla.org/MPL/2.0/.


use std::{mem, thread};
use std::io::{Error, ErrorKind};
use std::time::Duration;
use std::cell::UnsafeCell;
use std::sync::{Arc, Mutex};
use std::net::{TcpStream, TcpListener};
use std::os::unix::io::{RawFd, AsRawFd, IntoRawFd};

use libc;
use errno::errno;
use threadpool::ThreadPool;
use simple_slab::Slab;

use types::*;
use config::Config;
use super::Handler;


// When added to epoll, these will be the conditions of kernel notification:
//
// EPOLLIN          - Data is available in kernel buffer.
// EPOLLRDHUP       - Peer closed connection.
// EPOLLET          - Register in EdgeTrigger mode.
// EPOLLONESHOT     - After an event is pulled out with epoll_wait(2) the associated
//                    file descriptor is internally disabled and no other events will
//                    be reported by the epoll interface.
const DEFAULT_EVENTS: i32 = libc::EPOLLIN |
                            libc::EPOLLRDHUP |
                            libc::EPOLLET |
                            libc::EPOLLONESHOT;

// Maximum number of events returned from epoll_wait
const MAX_EVENTS: i32 = 100;

// Useful to keep from passing a copy of a RawFd everywhere
#[allow(non_upper_case_globals)]
static mut epfd: RawFd = 0 as RawFd;

pub fn begin(handler: Box<dyn Handler>, cfg: Config) {
    info!("Starting server...");

    // Wrap handler in something we can share between threads
    let event_handler = EventHandler(Box::into_raw(handler));

    // Create our new connections slab
    let new_connection_slab = Arc::new(Mutex::new(Slab::<Connection>::with_capacity(10)));

    // Create our connection slab
    let mut_slab = MutSlab {
        inner: UnsafeCell::new(Slab::<Arc<Connection>>::with_capacity(cfg.pre_allocated))
    };
    let connection_slab = Arc::new(mut_slab);

    // Start the event loop
    let threads = cfg.max_threads;
    let eh_clone = event_handler.clone();
    let new_connections = new_connection_slab.clone();
    unsafe {
        thread::Builder::new()
            .name("Event Loop".to_string())
            .spawn(move || {
                event_loop(new_connections, connection_slab, eh_clone, threads)
            })
            .unwrap();
    }

    // Start the TcpListener loop
    let eh_clone = event_handler.clone();
    let listener_thread = unsafe {
        thread::Builder::new()
            .name("TcpListener Loop".to_string())
            .spawn(move || { listener_loop(cfg, new_connection_slab, eh_clone) })
            .unwrap()
    };
    let _ = listener_thread.join();
}

unsafe fn listener_loop(cfg: Config, new_connections: NewConnectionSlab, handler: EventHandler) {
    info!("Starting incoming TCP connection listener...");
    let listener_result = TcpListener::bind((&cfg.addr[..], cfg.port));
    if listener_result.is_err() {
        let err = listener_result.unwrap_err();
        error!("Creating TcpListener: {}", err);
        panic!();
    }

    let listener = listener_result.unwrap();
    setup_listener_options(&listener, handler.clone());

    info!("Incoming TCP conecction listener started");

    for accept_attempt in listener.incoming() {
        match accept_attempt {
            Ok(tcp_stream) => handle_new_connection(tcp_stream, &new_connections, handler.clone()),
            Err(e) => error!("Accepting connection: {}", e)
        };
    }

    debug!("Dropping TcpListener");

    drop(listener);
}

unsafe fn setup_listener_options(listener: &TcpListener, handler: EventHandler) {
    info!("Setting up listener options");
    let fd = listener.as_raw_fd();
    let EventHandler(handler_ptr) = handler;

    (*handler_ptr).on_server_created(fd);
}

unsafe fn handle_new_connection(tcp_stream: TcpStream,
                                new_connections: &NewConnectionSlab,
                                handler: EventHandler)
{
    debug!("New connection received");
    // Take ownership of tcp_stream's underlying file descriptor
    let fd = tcp_stream.into_raw_fd();

    // Execute EventHandler's constructor
    let EventHandler(handler_ptr) = handler;
    let arc_stream = (*handler_ptr).on_new_connection(fd);

    // Create a connection structure
    let connection = Connection {
        fd: fd,
        err_mutex: Mutex::new(None),
        tx_mutex: Mutex::new(()),
        stream: arc_stream
    };

    // Insert it into the NewConnectionSlab
    let mut slab = match (*new_connections).lock() {
        Ok(g) => g,
        Err(p) => p.into_inner()
    };

    (&mut *slab).insert(connection);
}

/// Main event loop
unsafe fn event_loop(new_connections: NewConnectionSlab,
                     connection_slab: ConnectionSlab,
                     handler: EventHandler,
                     threads: usize)
{
    info!("Event loop starting...");
    const MAX_WAIT: i32 = 1000; // Milliseconds

    info!("Creating epoll instance...");
    // Attempt to create an epoll instance
    let result = libc::epoll_create(1);
    if result < 0 {
        let err = Error::from_raw_os_error(errno().0 as i32);
        error!("Creating epoll instance: {}", err);
        panic!();
    }

    // Epoll instance
    epfd = result;
    info!("Epoll instance created with fd: {}", epfd);

    info!("Creating I/O threadpool with {} threads", threads);

    // ThreadPool with user specified number of threads
    let thread_pool = ThreadPool::new(threads);

    // Our I/O queue for Connections needing various I/O operations.
    let arc_io_queue = Arc::new(Mutex::new(Vec::<IoPair>::with_capacity(MAX_EVENTS as usize)));

    // Start the I/O Sentinel
    let t_pool_clone = thread_pool.clone();
    let handler_clone = handler.clone();
    let io_queue = arc_io_queue.clone();
    thread::Builder::new()
        .name("I/O Sentinel".to_string())
        .spawn(move || {
            io_sentinel(io_queue, t_pool_clone, handler_clone)
        })
        .unwrap();

    // Scratch space for epoll returned events
    let mut event_buffer = Vec::<libc::epoll_event>::with_capacity(MAX_EVENTS as usize);
    event_buffer.set_len(MAX_EVENTS as usize);

    info!("Starting epoll_wait loop...");
    loop {
        // Remove any connections in an error'd state.
        remove_stale_connections(&connection_slab, &thread_pool, &handler);

        // Insert any newly received connections into the connection_slab
        insert_new_connections(&new_connections, &connection_slab);

        // Check for any new events
        let result = libc::epoll_wait(epfd, event_buffer.as_mut_ptr(), MAX_EVENTS, MAX_WAIT);
        if result < 0 {
            let err = Error::from_raw_os_error(errno().0 as i32);
            error!("During epoll_wait: {}", err);
            panic!();
        }

        let num_events = result as usize;
        update_io_events(&connection_slab, &arc_io_queue, &event_buffer[0..num_events]);
    }
}

/// Traverses through the connection slab and creates a list of connections that need dropped,
/// then traverses that list, drops them, and informs the handler of client drop.
unsafe fn remove_stale_connections(connection_slab: &ConnectionSlab,
                                   thread_pool: &ThreadPool,
                                   handler: &EventHandler)
{
    let slab_ptr = (*connection_slab).inner.get();

    let mut x: isize = 0;
    while x < (*slab_ptr).len() as isize {
        let err_state = {
            let state = (*slab_ptr)[x as usize].err_mutex.lock().unwrap();
            if state.is_some() {
                let err_kind = (*state).as_ref().unwrap().kind();
                let err_desc = (*state).as_ref().unwrap().to_string();
                Some(Error::new(err_kind, err_desc))
            } else {
                None
            }
        };

        err_state.map(|e| {
            trace!("Found stale connection");

            let arc_connection = (*slab_ptr).remove(x as usize);
            close_connection(&arc_connection);

            let fd = arc_connection.fd;
            let handler_clone = (*handler).clone();
            thread_pool.execute(move || {
                let EventHandler(ptr) = handler_clone;
                (*ptr).on_connection_removed(fd, e);
            });
            x -= 1;
        });

        x += 1;
    }
}

/// Closes the connection's underlying file descriptor
unsafe fn close_connection(connection: &Arc<Connection>) {
    let fd = (*connection).fd;
    debug!("Closing fd: {}", fd);

    let result = libc::close(fd);
    if result < 0 {
        let err = Error::from_raw_os_error(errno().0 as i32);
        error!("Closing fd: {}    {}", fd, err);
    }
}

/// Transfers Connections from the new_connections slab to the "main" connection_slab.
unsafe fn insert_new_connections(new_connections: &NewConnectionSlab,
                                 connection_slab: &ConnectionSlab)
{
    let mut new_slab = match new_connections.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner()
    };

    let num_connections = (&mut *new_slab).len();
    let arc_main_slab = (*connection_slab).inner.get();
    for _ in 0..num_connections {
        let connection = (&mut *new_slab).remove(0);
        let arc_connection = Arc::new(connection);
        (*arc_main_slab).insert(arc_connection.clone());
        add_connection_to_epoll(&arc_connection);
    }
}

/// Adds a new connection to the epoll interest list.
unsafe fn add_connection_to_epoll(arc_connection: &Arc<Connection>) {
    let fd = (*arc_connection).fd;
    debug!("Adding fd {} to epoll", fd);
    let result = libc::epoll_ctl(epfd,
                                 libc::EPOLL_CTL_ADD,
                                 fd,
                                 &mut libc::epoll_event {
                                     events: DEFAULT_EVENTS as u32,
                                     u64: fd as u64
                                 });

    if result < 0 {
        let err = Error::from_raw_os_error(errno().0 as i32);
        error!("Adding fd: {} to epoll:   {}", fd, err);

        let mut err_state = match arc_connection.err_mutex.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner()
        };

        *err_state = Some(err);
    }
}

/// Re-arms a connection in the epoll interest list with the event mask.
unsafe fn rearm_connection_in_epoll(arc_connection: &Arc<Connection>, flags: i32) {
    let fd = arc_connection.fd;
    let events = DEFAULT_EVENTS | flags;

    trace!("EPOLL_CTL_MOD   fd: {}    flags: {:#b}", fd, (flags as u32));

    let result = libc::epoll_ctl(epfd,
                                 libc::EPOLL_CTL_MOD,
                                 fd,
                                 &mut libc::epoll_event { events: events as u32, u64: fd as u64 });

    if result < 0 {
        let err = Error::from_raw_os_error(errno().0 as i32);
        error!("EPOLL_CTL_MOD   fd: {}    {}", fd, err);

        let mut err_state = match arc_connection.err_mutex.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner()
        };

        *err_state = Some(err);
    }
}

/// Traverses the ConnectionSlab and updates any connection's state reported changed by epoll.
unsafe fn update_io_events(connection_slab: &ConnectionSlab,
                           arc_io_queue: &IoQueue,
                           events: &[libc::epoll_event])
{
    const READ_EVENT: u32 = libc::EPOLLIN as u32;
    const WRITE_EVENT: u32 = libc::EPOLLOUT as u32;
    const CLOSE_EVENT: u32 = (libc::EPOLLRDHUP | libc::EPOLLERR | libc::EPOLLHUP) as u32;

    for event in events.iter() {
        // Locate the connection this event is for
        let fd = event.u64 as RawFd;

        let flags = event.events;

        trace!("Epoll event for fd: {fd}    flags: {flags:#b}");

        let find_result = find_connection_from_fd(fd, connection_slab);
        if find_result.is_err() {
            error!("Unable to find fd {} in ConnectionSlab", fd);
            continue;
        }

        let arc_connection = find_result.unwrap();

        // Error/hangup occurred?
        let close_event = (event.events & CLOSE_EVENT) > 0;
        if close_event {
            { // Mutex lock
                let mut err_opt = match arc_connection.err_mutex.lock() {
                    Ok(g) => g,
                    Err(p) => p.into_inner()
                };

                *err_opt = Some(Error::new(ErrorKind::ConnectionAborted, "ConnectionAborted"));
            } // Mutex unlock
            continue;
        }

        // Read or write branch
        let read_available = (event.events & READ_EVENT) > 0;
        let write_available = (event.events & WRITE_EVENT) > 0;
        if !read_available && !write_available {
            trace!("Event was neither read nor write: assuming hangup");
            { // Mutex lock
                let mut err_opt = match arc_connection.err_mutex.lock() {
                    Ok(g) => g,
                    Err(p) => p.into_inner()
                };

                *err_opt = Some(Error::new(ErrorKind::ConnectionAborted, "ConnectionAborted"));
            } // Mutex unlock
            continue;
        }

        let io_event = if read_available && write_available {
            trace!("Event: RW");
            IoEvent::ReadWriteAvailable
        } else if read_available {
            trace!("Event: R");
            IoEvent::ReadAvailable
        } else {
            trace!("Event W");
            IoEvent::WriteAvailable
        };

        let io_pair = IoPair {
            event: io_event,
            arc_connection: arc_connection
        };

        trace!("Adding event to queue");
        { // Mutex lock
            let mut io_queue = match arc_io_queue.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner()
            };

            (*io_queue).push(io_pair);
        } // Mutex unlock
    }
}

/// Given a fd and ConnectionSlab, returns the Connection associated with fd.
unsafe fn find_connection_from_fd(fd: RawFd,
                                  connection_slab: &ConnectionSlab)
                                  -> Result<Arc<Connection>, ()>
{
    let slab_ptr = (*connection_slab).inner.get();
    for ref arc_connection in (*slab_ptr).iter() {
        if (*arc_connection).fd == fd {
            return Ok((*arc_connection).clone());
        }
    }

    Err(())
}

unsafe fn io_sentinel(arc_io_queue: IoQueue, thread_pool: ThreadPool, handler: EventHandler) {
    info!("Starting I/O Sentinel");
    // We want to wake up with the same interval consitency as the epoll_wait loop.
    // Plus a few ms for hopeful non-interference from mutex contention.
    let _100ms = 1000000 * 100;
    let wait_interval = Duration::new(0, _100ms);

    loop {
        thread::sleep(wait_interval);

        let io_queue;
        { // Mutex lock
            let mut queue = match arc_io_queue.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner()
            };

            let empty_queue = Vec::<IoPair>::with_capacity(MAX_EVENTS as usize);
            io_queue = mem::replace(&mut (*queue), empty_queue);
        } // Mutex unlock

        if io_queue.len() > 0 {
        trace!("Processing {} I/O events", io_queue.len());
    }

    for ref io_pair in io_queue.iter() {
        let io_event = io_pair.event.clone();
        let handler_clone = handler.clone();
        let arc_connection = io_pair.arc_connection.clone();
        thread_pool.execute(move || {
            let mut rearm_events = 0i32;
            if io_event == IoEvent::WriteAvailable
                || io_event == IoEvent::ReadWriteAvailable
            {
                let flags = handle_write_event(arc_connection.clone());
                if flags == -1 {
                    return;
                }
                rearm_events |= flags;
            }
            if io_event == IoEvent::ReadAvailable
                || io_event == IoEvent::ReadWriteAvailable
            {
                let flags = handle_read_event(arc_connection.clone(), handler_clone);
                if flags == -1 {
                    return;
                }
                rearm_events |= flags;
            }

            rearm_connection_in_epoll(&arc_connection, rearm_events);
        });
    }
}
}

/// Handles an EPOLLOUT event. An empty buffer is sent down the tx line to
/// force whatever was left in the tx_buffer into the kernel's outbound buffer.
unsafe fn handle_write_event(arc_connection: Arc<Connection>) -> i32 {
    debug!("Handling a write backlog event...");
    let err;
    { // Mutex lock
        drop(match arc_connection.tx_mutex.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner()
        });

        // Get a pointer into UnsafeCell<Stream>
        let stream_ptr = arc_connection.stream.get();

        let empty = Vec::<u8>::new();
        let write_result = (*stream_ptr).send(&empty[..]);
        if write_result.is_ok() {
            debug!("Cleared backlog");
            return 0i32;
        }

        err = write_result.unwrap_err();
        if err.kind() == ErrorKind::WouldBlock {
            debug!("Backlog still not cleared, returning EPOLLOUT flags for fd");
            return libc::EPOLLOUT;
        }
    } // Mutex unlock

    { // Mutex lock
        let mut err_state = match arc_connection.err_mutex.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner()
        };

        *err_state = Some(err);
    } // Mutex unlock

    return -1i32;
}

unsafe fn handle_read_event(arc_connection: Arc<Connection>, handler: EventHandler) -> i32 {
    trace!("Handling read event");
    let stream_ptr = arc_connection.stream.get();

    // Attempt recv
    match (*stream_ptr).recv() {
        Ok(mut queue) => {
            trace!("Read {} msgs", queue.len());
            for msg in queue.drain(..) {
                let EventHandler(ptr) = handler;
                let hydrogen_socket = HydrogenSocket::new(arc_connection.clone(),
                                                          rearm_connection_in_epoll);
                (*ptr).on_data_received(hydrogen_socket, msg);
            }
            return libc::EPOLLIN;
        }

        Err(err) => {
            let kind = err.kind();
            if kind == ErrorKind::WouldBlock {
                trace!("ErrorKind::WouldBlock");
                return libc::EPOLLIN;
            }

            if kind != ErrorKind::UnexpectedEof
                && kind != ErrorKind::ConnectionReset
                && kind != ErrorKind::ConnectionAborted
            {
                error!("Unexpected during recv:   {}", err);
            } else {
                debug!("Received during read:    {}", err);
            }

            { // Mutex lock
                // If we're in a state of ShouldClose, no need to worry
                // about any other operations...
                let mut err_state = match arc_connection.err_mutex.lock() {
                    Ok(g) => g,
                    Err(p) => p.into_inner()
                };

                *err_state = Some(err);
            } // Mutex unlock
        }
    };

    return -1i32;
}
