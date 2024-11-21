use nix::errno::Errno;
use once_cell::sync::Lazy;
use std::alloc::{Layout, alloc_zeroed};
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::Mutex;

static SEND_BUFFER: Lazy<Mutex<Option<BufferType>>> = Lazy::new(|| Mutex::new(None));
static RECEIVE_BUFFER: Lazy<Mutex<Option<BufferType>>> = Lazy::new(|| Mutex::new(None));

enum BufferType {
    MemAligned(Vec<u8>),
    Default(Vec<u8>),
}

impl BufferType {
    fn into(&self) -> &Vec<u8> {
        match self {
            BufferType::MemAligned(data) | BufferType::Default(data) => data,
        }
    }
}

pub unsafe fn libspdm_setup_pagealigned_io_buffers(
    context: *mut c_void,
    send_recv_len: usize,
    libsdpm_buff_len: usize,
) -> Result<(), Errno> {
    if !send_recv_len.is_multiple_of(page_size::get()) {
        error!("Requested SEND/RECV buffer size is not target page size aligned");
        return Err(Errno::EINVAL);
    }

    // The buffers are page aligned, useful for NVMe userspace API buffers.
    fn page_aligned_buffer(size: usize) -> Result<Vec<u8>, Errno> {
        let layout = Layout::from_size_align(size, page_size::get()).map_err(|_| Errno::EINVAL)?;
        let ptr = unsafe { alloc_zeroed(layout) };
        let ptr = NonNull::new(ptr).ok_or(Errno::ENOMEM)?;
        // Safe because layout size matches Vec length/capacity
        let vec = unsafe { Vec::from_raw_parts(ptr.as_ptr(), size, size) };
        Ok(vec)
    }

    let buffer_send = page_aligned_buffer(send_recv_len)?;
    let buffer_recv = page_aligned_buffer(send_recv_len)?;

    *(SEND_BUFFER.lock().unwrap()) = Some(BufferType::MemAligned(buffer_send));
    *(RECEIVE_BUFFER.lock().unwrap()) = Some(BufferType::MemAligned(buffer_recv));

    unsafe {
        libspdm::libspdm_rs::libspdm_register_device_buffer_func(
            context,
            libsdpm_buff_len as u32,
            libsdpm_buff_len as u32,
            Some(acquire_sender_buffer),
            Some(release_sender_buffer),
            Some(acquire_receiver_buffer),
            Some(release_receiver_buffer),
        )
    };

    Ok(())
}

pub fn libspdm_setup_io_buffers(
    context: *mut c_void,
    send_len: usize,
    recv_len: usize,
) -> Result<(), ()> {
    let result = std::panic::catch_unwind(|| {
        let buffer_send = vec![0; send_len];
        let buffer_receive = vec![0; recv_len];
        (buffer_send, buffer_receive)
    });

    match result {
        Ok(buffers) => {
            *(SEND_BUFFER.lock().unwrap()) = Some(BufferType::Default(buffers.0));
            *(RECEIVE_BUFFER.lock().unwrap()) = Some(BufferType::Default(buffers.1));

            unsafe {
                libspdm::libspdm_rs::libspdm_register_device_buffer_func(
                    context,
                    send_len as u32,
                    recv_len as u32,
                    Some(acquire_sender_buffer),
                    Some(release_sender_buffer),
                    Some(acquire_receiver_buffer),
                    Some(release_receiver_buffer),
                )
            }
            Ok(())
        }
        Err(_) => {
            error!("Failed to allocate transport buffers");
            Err(())
        }
    }
}

/// # Summary
///
/// A helper function to capture the SEND_BUFFER into `msg_buf_ptr`
///
/// # Parameter
///
/// * `_context`: The SPDM context
/// * `max_msg_size`: Returns the length of the sender buffer
/// * `msg_buf_ptr`: Returns a pointer to the sender buffer (mutable)
///
/// # Returns
///
/// (0) on success
#[unsafe(no_mangle)]
pub unsafe extern "C" fn acquire_sender_buffer(
    _context: *mut c_void,
    msg_buf_ptr: *mut *mut c_void,
) -> u32 {
    if let Some(ref buf) = *SEND_BUFFER.lock().unwrap() {
        let buf_ptr = buf.into().as_ptr() as *mut c_void;
        unsafe { *msg_buf_ptr = buf_ptr };
        return 0;
    }
    error!("Sender buffer is lost or not initialized");
    1
}

/// # Summary
///
/// A helper function to capture the RECEIVE_BUFFER into `msg_buf_ptr`
///
/// # Parameter
///
/// * `_context`: The SPDM context
/// * `max_msg_size`: Returns the length of the receiver buffer
/// * `msg_buf_ptr`: Returns a pointer to the receiver buffer (mutable)
///
/// # Returns
///
/// (0) on success
#[unsafe(no_mangle)]
pub unsafe extern "C" fn acquire_receiver_buffer(
    _context: *mut c_void,
    msg_buf_ptr: *mut *mut c_void,
) -> u32 {
    if let Some(ref buf) = *RECEIVE_BUFFER.lock().unwrap() {
        let buf_ptr = buf.into().as_ptr() as *mut c_void;
        unsafe { *msg_buf_ptr = buf_ptr };
        return 0;
    }
    error!("Receiver buffer is lost or not initialized");
    1
}

/// We are only passing a reference to heap allocated memory, no-op required
#[unsafe(no_mangle)]
pub unsafe extern "C" fn release_receiver_buffer(
    _context: *mut c_void,
    _msg_buf_ptr: *const c_void,
) {
}

/// We are only passing a reference to heap allocated memory, no-op required
#[unsafe(no_mangle)]
pub unsafe extern "C" fn release_sender_buffer(_context: *mut c_void, _msg_buf_ptr: *const c_void) {
}

/// # Summary
///
/// Drop the IO buffers out of scope, this should release the underlying
/// memory.
pub unsafe fn libspdm_drop_io_buffers() {
    let mut send_buf = SEND_BUFFER.lock().unwrap();
    if let Some(buffer) = send_buf.take() {
        let mut data = match buffer {
            BufferType::MemAligned(vec) | BufferType::Default(vec) => vec,
        };
        let ptr = data.as_mut_ptr() as *mut c_void;
        // Forget to avoid, double free when this goes out of scope.
        std::mem::forget(data);
        unsafe {
            nix::libc::free(ptr);
        }
    } else {
        error!("Send buffer is lost or not initialized");
    }

    let mut recv_buf = RECEIVE_BUFFER.lock().unwrap();
    if let Some(buffer) = recv_buf.take() {
        let mut data = match buffer {
            BufferType::MemAligned(vec) | BufferType::Default(vec) => vec,
        };
        let ptr = data.as_mut_ptr() as *mut c_void;
        // Forget to avoid, double free when this goes out of scope.
        std::mem::forget(data);
        unsafe {
            nix::libc::free(ptr);
        }
    } else {
        error!("Receive buffer is lost or not initialized");
    }
}
