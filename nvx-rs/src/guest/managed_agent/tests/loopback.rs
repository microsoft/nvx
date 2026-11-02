use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::guest::sys::{self, Errno, FileDescriptor};
use crate::host::platform::Transport;
use crate::protocol::{
    self, OUTER_ACK, OUTER_CREDIT, OUTER_DATA, OUTER_GUEST_ATTACH, OUTER_HEADER_LEN,
    OUTER_HOST_ATTACH, OUTER_READY, OUTER_RESET, OuterHeader,
};

use super::seam::tests::{TestSystem, setpriv_shim, spawn_as_privileged};

struct BrokerCore {
    guest: UnixStream,
    host_output: VecDeque<u8>,
    instance_id: [u8; 16],
    epoch: u64,
    initialized: bool,
    active_host: bool,
    host_receive_sequence: u64,
    host_send_sequence: u64,
    guest_send_sequence: u64,
    guest_receive_sequence: u64,
    receive_window: u32,
    receive_credit: u32,
}

impl BrokerCore {
    fn new(guest: UnixStream) -> Self {
        Self {
            guest,
            host_output: VecDeque::new(),
            instance_id: core::array::from_fn(|index| index as u8 + 1),
            epoch: 0,
            initialized: false,
            active_host: false,
            host_receive_sequence: 0,
            host_send_sequence: 0,
            guest_send_sequence: 0,
            guest_receive_sequence: 0,
            receive_window: 0,
            receive_credit: 0,
        }
    }

    fn attach_host(&mut self, header: OuterHeader, payload: &[u8]) -> io::Result<()> {
        if header.record_type != OUTER_HOST_ATTACH
            || header.instance_id != [0; 16]
            || header.epoch != 0
            || header.sequence != 0
            || payload.len() != protocol::CAPABILITY_LEN
            || self.active_host
        {
            return Err(protocol_error());
        }
        self.host_output.clear();
        self.host_receive_sequence = 0;
        self.host_send_sequence = 1;
        if !self.initialized {
            let (attach, payload) = self.read_guest(Some(Duration::from_secs(5)))?;
            if attach.record_type != OUTER_GUEST_ATTACH
                || attach.instance_id != [0; 16]
                || attach.epoch != 0
                || attach.sequence != 0
                || !payload.is_empty()
            {
                return Err(protocol_error());
            }
            self.epoch = 1;
            self.reset_guest()?;
            self.initialized = true;
        }
        self.active_host = true;
        self.queue_host(OUTER_READY, 0, &[])
    }

    fn accept_host_data(&mut self, header: OuterHeader, payload: &[u8]) -> io::Result<()> {
        if !self.active_host
            || header.record_type != OUTER_DATA
            || header.instance_id != self.instance_id
            || header.epoch != self.epoch
            || header.sequence != self.host_receive_sequence
            || payload.is_empty()
            || payload.len() > self.receive_credit as usize
        {
            return Err(protocol_error());
        }
        self.host_receive_sequence = self.host_receive_sequence.wrapping_add(1);
        self.receive_credit -= payload.len() as u32;
        let record = protocol::encode_outer(
            OUTER_DATA,
            &self.instance_id,
            self.epoch,
            self.guest_send_sequence,
            payload,
        )
        .map_err(|_| protocol_error())?;
        self.guest_send_sequence = self.guest_send_sequence.wrapping_add(1);
        self.guest.write_all(&record)
    }

    fn pump_guest(&mut self, timeout: Option<Duration>) -> io::Result<()> {
        loop {
            let (header, payload) = self.read_guest(timeout)?;
            if header.instance_id != self.instance_id
                || header.epoch != self.epoch
                || header.sequence != self.guest_receive_sequence
            {
                return Err(protocol_error());
            }
            self.guest_receive_sequence = self.guest_receive_sequence.wrapping_add(1);
            match header.record_type {
                OUTER_CREDIT => {
                    let credit =
                        protocol::decode_credit_payload(&payload).map_err(|_| protocol_error())?;
                    let available = self
                        .receive_credit
                        .checked_add(credit)
                        .filter(|available| *available <= self.receive_window)
                        .ok_or_else(protocol_error)?;
                    self.receive_credit = available;
                }
                OUTER_DATA => {
                    self.queue_host(OUTER_DATA, self.host_send_sequence, &payload)?;
                    self.host_send_sequence = self.host_send_sequence.wrapping_add(1);
                    return Ok(());
                }
                _ => return Err(protocol_error()),
            }
        }
    }

    fn reset_active_host(&mut self) -> io::Result<()> {
        if !self.initialized {
            return Err(protocol_error());
        }
        self.epoch = self
            .epoch
            .checked_add(1)
            .ok_or_else(|| io::Error::from(Errno::EOVERFLOW))?;
        self.receive_credit = 0;
        self.reset_guest()?;
        // The RESET ends the host's attachment with the session's new identity, after the
        // records that the host has not read, at the next sequence number of the host's leg.
        let sequence = self.host_send_sequence;
        self.host_send_sequence = self.host_send_sequence.wrapping_add(1);
        self.queue_host(OUTER_RESET, sequence, &[])
    }

    fn reset_guest(&mut self) -> io::Result<()> {
        self.guest_send_sequence = 1;
        self.guest_receive_sequence = 0;
        let reset = protocol::encode_outer(OUTER_RESET, &self.instance_id, self.epoch, 0, &[])
            .map_err(|_| protocol_error())?;
        self.guest.write_all(&reset)?;
        loop {
            let (header, payload) = self.read_guest(Some(Duration::from_secs(5)))?;
            if header.record_type == OUTER_ACK
                && header.instance_id == self.instance_id
                && header.epoch == self.epoch
                && header.sequence == 0
            {
                let window =
                    protocol::decode_ack_payload(&payload).map_err(|_| protocol_error())?;
                self.receive_window = window;
                self.receive_credit = window;
                self.guest_receive_sequence = 1;
                return Ok(());
            }
            // The real broker discards records from the retired epoch while reset drains.
        }
    }

    fn queue_host(&mut self, record_type: u8, sequence: u64, payload: &[u8]) -> io::Result<()> {
        let record = protocol::encode_outer(
            record_type,
            &self.instance_id,
            self.epoch,
            sequence,
            payload,
        )
        .map_err(|_| protocol_error())?;
        self.host_output.extend(record);
        Ok(())
    }

    fn read_guest(&mut self, timeout: Option<Duration>) -> io::Result<(OuterHeader, Vec<u8>)> {
        self.guest.set_read_timeout(timeout)?;
        let mut header = [0u8; OUTER_HEADER_LEN];
        self.guest.read_exact(&mut header).map_err(timeout_error)?;
        let header = protocol::decode_outer_header(&header).map_err(|_| protocol_error())?;
        let mut payload = vec![0; header.length];
        self.guest.read_exact(&mut payload).map_err(timeout_error)?;
        Ok((header, payload))
    }
}

struct BrokerTransport {
    core: Arc<Mutex<BrokerCore>>,
}

impl Drop for BrokerTransport {
    fn drop(&mut self) {
        if let Ok(mut core) = self.core.lock() {
            core.active_host = false;
        }
    }
}

impl Transport for BrokerTransport {
    fn read(&mut self, buffer: &mut [u8], timeout: Option<Duration>) -> io::Result<usize> {
        let mut core = self.core.lock().unwrap();
        if core.host_output.is_empty() {
            core.pump_guest(timeout)?;
        }
        let count = buffer.len().min(core.host_output.len());
        for byte in &mut buffer[..count] {
            *byte = core.host_output.pop_front().expect("the count is bounded");
        }
        Ok(count)
    }

    fn write_all(&mut self, data: &[u8], _timeout: Option<Duration>) -> io::Result<()> {
        if data.len() < OUTER_HEADER_LEN {
            return Err(protocol_error());
        }
        let header = protocol::decode_outer_header(
            data[..OUTER_HEADER_LEN]
                .try_into()
                .map_err(|_| protocol_error())?,
        )
        .map_err(|_| protocol_error())?;
        let payload = data
            .get(OUTER_HEADER_LEN..)
            .filter(|payload| payload.len() == header.length)
            .ok_or_else(protocol_error)?;
        let mut core = self.core.lock().unwrap();
        match header.record_type {
            OUTER_HOST_ATTACH => core.attach_host(header, payload),
            OUTER_DATA => core.accept_host_data(header, payload),
            _ => Err(protocol_error()),
        }
    }
}

#[derive(Clone)]
struct BrokerHandle {
    core: Arc<Mutex<BrokerCore>>,
}

impl BrokerHandle {
    fn transport(&self) -> Box<dyn Transport> {
        Box::new(BrokerTransport {
            core: Arc::clone(&self.core),
        })
    }

    fn reset(&self) {
        self.core.lock().unwrap().reset_active_host().unwrap();
    }
}

fn protocol_error() -> io::Error {
    io::Error::from(Errno::EPROTO)
}

fn timeout_error(error: io::Error) -> io::Error {
    if matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    ) {
        io::ErrorKind::TimedOut.into()
    } else {
        error
    }
}

#[cfg(test)]
mod tests;
