// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Descriptor-chain payload adapters for FUSE dispatch (strategy A).
//!
//! Copied ~verbatim from OpenVMM `virtiofs/src/virtio_util.rs` (rev `3d1207b`,
//! MIT) — that module is private upstream but uses only public
//! `virtio`/`fuse`/`guestmem` API, and it is exactly the seam our offload device
//! needs to parse requests from / write replies into a descriptor chain. Renames
//! only (`VirtioPayloadReader` → [`PayloadReader`], `VirtioPayloadWriter` →
//! [`PayloadWriter`]); keep the bodies diffable against upstream.
//! [`PayloadReplySender`] mirrors upstream's `VirtioReplySender`
//! (`virtiofs/src/virtio.rs:303-328`).

use guestmem::GuestMemory;
use std::cmp;
use std::io;
use std::io::Read;
use std::io::Write;
use virtio::queue::VirtioQueuePayload;
use virtio::VirtioQueueCallbackWork;

/// An implementation of `Read` that allows reading data from a virtio payload that may use
/// multiple buffers.
pub(crate) struct PayloadReader<'payload, 'mem> {
    guest_memory: &'mem GuestMemory,
    payload: std::slice::Iter<'payload, VirtioQueuePayload>,
    current: Option<&'payload VirtioQueuePayload>,
    offset: usize,
    position: usize,
    len: usize,
}

impl<'payload, 'mem> PayloadReader<'payload, 'mem> {
    /// Create a new reader for the specified payload.
    pub fn new(guest_memory: &'mem GuestMemory, work: &'payload VirtioQueueCallbackWork) -> Self {
        let mut reader = Self {
            guest_memory,
            payload: work.payload.iter(),
            current: None,
            offset: 0,
            position: 0,
            len: work.get_payload_length(false) as usize,
        };

        reader.next_payload();
        reader
    }

    /// Skip ahead to the next readable payload.
    fn next_payload(&mut self) {
        // It would be nice to use filter instead when assigning to self.payload in the ctor, but
        // I can't figure out how to store the result of that (the type it returns is generic over
        // the callback type, and I don't want to use Box<dyn Iterator> if I can avoid it).
        self.current = self.payload.find(|p| !p.writeable);
    }

    /// Gets the current payload, or None if the end was reached.
    ///
    /// This function takes care of moving to the next payload buffer if the current offset matches
    /// the end.
    fn get_payload(&mut self) -> Option<&'payload VirtioQueuePayload> {
        let payload = self.current?;
        if self.offset == payload.length as usize {
            self.next_payload();
            self.offset = 0;
        }

        self.current
    }

    /// Gets the remaining length of the current payload buffer only.
    fn get_current_remaining_len(&mut self) -> usize {
        if let Some(payload) = self.get_payload() {
            payload.length as usize - self.offset
        } else {
            0
        }
    }
}

impl Read for PayloadReader<'_, '_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if let Some(payload) = self.get_payload() {
            // Determine how much space is left in the current buffer, and read at most that much.
            // A single call to read won't cross payload buffers, so to read more you must call
            // it repeatedly or e.g. use read_exact.
            let remaining = payload.length as usize - self.offset;
            let size = cmp::min(remaining, buf.len());
            self.guest_memory
                .read_at(payload.address + self.offset as u64, &mut buf[..size])
                .map_err(io::Error::other)?;
            self.offset += size;
            self.position += size;
            Ok(size)
        } else {
            Ok(0)
        }
    }
}

impl fuse::RequestReader for PayloadReader<'_, '_> {
    fn read_until(&mut self, byte: u8) -> lx::Result<Vec<u8>> {
        // Unlike with a simple slice, we can't just scan ahead for the desired byte. Instead,
        // repeatedly read some data and see if it contains the byte. It is expected that in most
        // cases, the string will be contained in the remainder of the current payload buffer,
        // in which case this loop only has one iteration.
        let mut buffer: Vec<u8> = Vec::new();
        let mut buffer_offset = 0;
        loop {
            // Read the rest of the current payload buffer.
            let len = self.get_current_remaining_len();
            if len == 0 {
                return Err(lx::Error::EINVAL);
            }

            buffer.resize_with(buffer.len() + len, Default::default);
            let start_offset = self.offset;
            assert!(self.read(&mut buffer[buffer_offset..])? == len);

            // Search for a matching byte in the portion of the buffer we just read.
            if let Some(length) = buffer[buffer_offset..].iter().position(|&c| c == byte) {
                // Return up to the matching byte.
                buffer.truncate(buffer_offset + length);

                // Rewind the offset to be just after the matching byte.
                self.offset = start_offset + length + 1;
                return Ok(buffer);
            } else {
                buffer_offset += len;
            }
        }
    }

    fn remaining_len(&self) -> usize {
        self.len - self.position
    }
}

/// An implementation of `Write` that allows writing data to virtio payload that may use multiple
/// buffers.
pub(crate) struct PayloadWriter<'payload, 'mem> {
    guest_memory: &'mem GuestMemory,
    payload: std::slice::Iter<'payload, VirtioQueuePayload>,
    current: Option<&'payload VirtioQueuePayload>,
    offset: usize,
}

impl<'payload, 'mem> PayloadWriter<'payload, 'mem> {
    /// Create a new writer for the specified work.
    pub fn new(guest_memory: &'mem GuestMemory, work: &'payload VirtioQueueCallbackWork) -> Self {
        let mut writer = Self {
            guest_memory,
            payload: work.payload.iter(),
            current: None,
            offset: 0,
        };

        writer.next_payload();
        writer
    }

    /// Skip ahead to the next writable payload buffer.
    fn next_payload(&mut self) {
        self.current = self.payload.find(|p| p.writeable);
    }

    /// Gets the current payload, or None if the end was reached.
    ///
    /// This function takes care of moving to the next payload buffer if the current offset matches
    /// the end.
    fn get_payload(&mut self) -> Option<&'payload VirtioQueuePayload> {
        let payload = self.current?;
        if self.offset == payload.length as usize {
            self.next_payload();
            self.offset = 0;
        }

        self.current
    }
}

impl Write for PayloadWriter<'_, '_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Some(payload) = self.get_payload() {
            // Find the remaining size of the current payload buffer, and write at most that much.
            // This method never writes data spanning multiple buffers, so to write more you must
            // call it repeatedly or use write_all.
            let remaining = payload.length as usize - self.offset;
            let size = cmp::min(remaining, buf.len());
            self.guest_memory
                .write_at(payload.address + self.offset as u64, &buf[..size])
                .map_err(io::Error::other)?;
            self.offset += size;
            Ok(size)
        } else {
            Ok(0)
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// An implementation of `ReplySender` for virtio payload.
///
/// Writes the FUSE reply into guest memory and records the byte count.
/// Does not complete the descriptor — the caller is responsible for that
/// (and for FUSE no-reply operations `send` is never called, so
/// `bytes_written` stays 0).
pub(crate) struct PayloadReplySender<'a> {
    work: &'a VirtioQueueCallbackWork,
    mem: &'a GuestMemory,
    pub bytes_written: u32,
}

impl<'a> PayloadReplySender<'a> {
    pub fn new(mem: &'a GuestMemory, work: &'a VirtioQueueCallbackWork) -> Self {
        Self {
            work,
            mem,
            bytes_written: 0,
        }
    }
}

impl fuse::ReplySender for PayloadReplySender<'_> {
    fn send(&mut self, bufs: &[io::IoSlice<'_>]) -> io::Result<()> {
        let mut writer = PayloadWriter::new(self.mem, self.work);
        let mut size = 0;

        // Write all the slices to the payload buffers.
        // N.B. write_vectored isn't used because it isn't guaranteed to write all the data.
        for buf in bufs {
            writer.write_all(buf)?;
            size += buf.len();
        }

        self.bytes_written = size as u32;
        Ok(())
    }
}
