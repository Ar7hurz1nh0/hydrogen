// Copyright 2015 Nathan Sizemore <nathanrsizemore@gmail.com>
//
// This Source Code Form is subject to the
// terms of the Mozilla Public License, v.
// 2.0. If a copy of the MPL was not
// distributed with this file, You can
// obtain one at
// http://mozilla.org/MPL/2.0/.
//
// This Source Code Form is "Incompatible
// With Secondary Licenses", as defined by
// the Mozilla Public License, v. 2.0.


use std::{str};
use std::ffi::{CStr, CString};
use std::thread;
use std::net::TcpStream;
use std::sync::mpsc::{channel, Sender, Receiver};

use super::libc::{c_int, c_char};
use super::simple_stream::bstream::Bstream;


/// This client's writer channel
static mut writer_tx: *mut Sender<Vec<u8>> = 0 as *mut Sender<Vec<u8>>;

/// This client's kill channel
static mut kill_tx: *mut Sender<()> = 0 as *mut Sender<()>;


#[no_mangle]
pub extern "C" fn hydrogen_start(address: *const c_char,
    data_handler: extern fn(*const c_char),
    on_connect_handler: extern fn(),
    on_disconnect_handler: extern fn()) -> c_int {

    // TODO - adjust this to accept a log level adjustable by whoever is running
    // the application
    super::init();

    trace!("Rust - start()");

    let mut r_address;
    unsafe {
        r_address = CStr::from_ptr(address);
    }
    let s_address = r_address.to_bytes();
    let host_address = match str::from_utf8(s_address) {
        Ok(safe_str) => safe_str,
        Err(_) => {
            error!("Invalid host address");
            return -1 as c_int;
        }
    };
    trace!("Address: ");

    // Create and register a way to kill this client
    let (k_tx, kill_rx): (Sender<()>, Receiver<()>) = channel();
    unsafe { *kill_tx = k_tx.clone(); }

    // // Writer thread's channel
    // let (w_tx, writer_rx): (Sender<Vec<u8>>, Receiver<Vec<u8>>) = channel();
    // unsafe { *writer_tx = w_tx.clone(); }
    //
    // debug!("Attempting connect to: {}", host_address);
    //
    // let result = TcpStream::connect(host_address);
    // if result.is_err() {
    //     error!("Error connecting to {} - {}", host_address, result.unwrap_err());
    //     return -1 as c_int;
    // }
    // debug!("Connected");
    // on_connect_handler();
    //
    // let stream = result.unwrap();
    // let client = Bstream::new(stream);
    // let r_client = client.clone();
    // let w_client = client.clone();
    //
    // // Start the reader thread
    // thread::Builder::new()
    //     .name("ReaderThread".to_string())
    //     .spawn(move||{
    //         reader_thread(r_client, data_handler)
    //     }).unwrap();
    //
    // // Start the writer thread
    // thread::Builder::new()
    //     .name("WriterThread".to_string())
    //     .spawn(move||{
    //         writer_thread(writer_rx, w_client)
    //     }).unwrap();
    //
    // // Wait for the kill signal
    // match kill_rx.recv() {
    //     Ok(_) => { }
    //     Err(e) => {
    //         error!("Error on kill channel: {}", e);
    //         on_disconnect_handler();
    //         return -1 as c_int;
    //     }
    // };
    // on_disconnect_handler();

    // Exit out in standard C fashion
    0 as c_int
}

/// Writes the complete contents of buffer to the server
/// Returns -1 on error
#[no_mangle]
pub extern "C" fn hydrogen_write(buffer: *const c_char) -> c_int {
    trace!("Rust.write");

    let mut buf_as_cstr;
    unsafe {
        buf_as_cstr = CStr::from_ptr(buffer);
    }
    let buf_as_slice = buf_as_cstr.to_bytes();

    let mut n_buffer = Vec::<u8>::with_capacity(buf_as_slice.len());
    for byte in buf_as_slice.iter() {
        n_buffer.push(*byte);
    }

    unsafe {
        match (*writer_tx).send(n_buffer) {
            Ok(_) => { }
            Err(e) => {
                warn!("Error sending buffer: {}", e);
                let _ = (*kill_tx).send(());
                return -1 as c_int;
            }
        };
    }

    0 as c_int
}

/// Forever listens to incoming data and when a complete message is received,
/// the passed callback is hit
fn reader_thread(client: Bstream, handler: extern fn(*const c_char)) {
    trace!("Rust.reader_thread started");

    let mut reader = client.clone();
    loop {
        match reader.read() {
            Ok(buffer) => {
                // Launch the handler in a new thread
                thread::Builder::new()
                    .name("Reader-Worker".to_string())
                    .spawn(move||{
                        let slice = &buffer[..];
                        let c_buffer = CString::new(slice).unwrap();
                        handler(c_buffer.as_ptr());
                    }).unwrap();
            }
            Err(e) => {
                error!("Error: {}", e);
                break;
            }
        };
    }
    debug!("Rust.reader_thread finished");
    unsafe { let _ = (*kill_tx).send(()); }
}

/// Forever listens to Receiver<Vec<u8>> waiting on messages to come in
/// Once available, blocks until the entire message has been written
fn writer_thread(rx: Receiver<Vec<u8>>, client: Bstream) {
    trace!("Rust.writer_thread started");

    let mut writer = client.clone();
    loop {
        match rx.recv() {
            Ok(ref mut buffer) => {
                match writer.write(buffer) {
                    Ok(_) => { }
                    Err(e) => {
                        error!("Error: {}", e);
                        break;
                    }
                };
            }
            Err(e) => {
                error!("Error: {}", e);
                break;
            }
        };
    }

    debug!("Rust.writer_thread finished");
    unsafe { let _ = (*kill_tx).send(()); }
}

/// Drops the current connection and kills all current threads
#[no_mangle]
pub extern "C" fn hydrogen_kill() {
    unsafe { let _ = (*kill_tx).send(()); }
}
