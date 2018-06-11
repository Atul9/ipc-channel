// Copyright 2015 The Servo Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use bincode;
use crossbeam_channel::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::collections::hash_map::HashMap;
use std::cell::{RefCell, Ref};
use std::io::{Error, ErrorKind};
use std::slice;
use std::fmt::{self, Debug, Formatter};
use std::cmp::{PartialEq};
use std::ops::{Deref, RangeFrom};
use std::usize;
use uuid::Uuid;

#[derive(Clone)]
struct ServerRecord {
    sender: OsIpcSender,
    conn_sender: Sender<bool>,
    conn_receiver: Receiver<bool>,
}

impl ServerRecord {
    fn new(sender: OsIpcSender) -> ServerRecord {
        let (tx, rx) = crossbeam_channel::unbounded::<bool>();
        ServerRecord {
            sender: sender,
            conn_sender: tx,
            conn_receiver: rx,
        }
    }

    fn accept(&self) {
        self.conn_receiver.recv().unwrap();
    }

    fn connect(&self) {
        self.conn_sender.send(true);
    }
}

lazy_static! {
    static ref ONE_SHOT_SERVERS: Mutex<HashMap<String,ServerRecord>> = Mutex::new(HashMap::new());
}

struct ChannelMessage(Vec<u8>, Vec<OsIpcChannel>, Vec<OsIpcSharedMemory>);

pub fn channel() -> Result<(OsIpcSender, OsIpcReceiver), ChannelError> {
    let (base_sender, base_receiver) = crossbeam_channel::unbounded::<ChannelMessage>();
    let is_disconnected = Arc::new(AtomicBool::new(false));
    Ok((
        OsIpcSender::new(base_sender, is_disconnected.clone()),
        OsIpcReceiver::new(base_receiver, is_disconnected)
    ))
}

#[derive(Debug)]
pub struct OsIpcReceiver(RefCell<Option<OsIpcReceiverInner>>);

#[derive(Debug)]
pub struct OsIpcReceiverInner {
    receiver: Receiver<ChannelMessage>,
    is_disconnected: Arc<AtomicBool>,
}

impl Drop for OsIpcReceiverInner {
    fn drop(&mut self) {
        self.is_disconnected.store(true, Ordering::SeqCst);
    }
}

impl PartialEq for OsIpcReceiver {
    fn eq(&self, other: &OsIpcReceiver) -> bool {
        self.0.borrow().as_ref().map(|rx| rx as *const _) ==
            other.0.borrow().as_ref().map(|rx| rx as *const _)
    }
}

impl OsIpcReceiver {
    fn new(receiver: Receiver<ChannelMessage>, is_disconnected: Arc<AtomicBool>) -> OsIpcReceiver {
        OsIpcReceiver(RefCell::new(Some(OsIpcReceiverInner { receiver, is_disconnected })))
    }

    pub fn consume(&self) -> OsIpcReceiver {
        OsIpcReceiver(RefCell::new(self.0.borrow_mut().take()))
    }

    pub fn recv(
        &self
    ) -> Result<(Vec<u8>, Vec<OsOpaqueIpcChannel>, Vec<OsIpcSharedMemory>), ChannelError> {
        let r = self.0.borrow();
        match r.as_ref().unwrap().receiver.recv() {
            Some(ChannelMessage(d, c, s)) => {
                Ok((d, c.into_iter().map(OsOpaqueIpcChannel::new).collect(), s))
            }
            None => Err(ChannelError::ChannelClosedError),
        }
    }

    pub fn try_recv(
        &self
    ) -> Result<(Vec<u8>, Vec<OsOpaqueIpcChannel>, Vec<OsIpcSharedMemory>), ChannelError> {
        let r = self.0.borrow();
        let r = &r.as_ref().unwrap().receiver;
        select! {
            recv(r, msg) => match msg {
                None => Err(ChannelError::ChannelClosedError),
                Some(ChannelMessage(d, c, s)) => {
                    Ok((d, c.into_iter().map(OsOpaqueIpcChannel::new).collect(), s))
                }
            }
            default => Err(ChannelError::UnknownError),
        }
    }
}

#[derive(Clone, Debug)]
pub struct OsIpcSender {
    sender: RefCell<Sender<ChannelMessage>>,
    is_disconnected: Arc<AtomicBool>,
}

impl PartialEq for OsIpcSender {
    fn eq(&self, other: &OsIpcSender) -> bool {
        &*self.sender.borrow() as *const _ ==
            &*other.sender.borrow() as *const _
    }
}

impl OsIpcSender {
    fn new(sender: Sender<ChannelMessage>, is_disconnected: Arc<AtomicBool>) -> OsIpcSender {
        OsIpcSender {
            sender: RefCell::new(sender),
            is_disconnected
        }
    }

    pub fn connect(name: String) -> Result<OsIpcSender, ChannelError> {
        let record = ONE_SHOT_SERVERS.lock().unwrap().get(&name).unwrap().clone();
        record.connect();
        Ok(record.sender)
    }

    pub fn get_max_fragment_size() -> usize {
        usize::MAX
    }

    pub fn send(
        &self,
        data: &[u8],
        ports: Vec<OsIpcChannel>,
        shared_memory_regions: Vec<OsIpcSharedMemory>,
    ) -> Result<(), ChannelError> {
        if self.is_disconnected.load(Ordering::SeqCst) {
            Err(ChannelError::BrokenPipeError)
        } else {
            Ok(self.sender
                .borrow()
                .send(ChannelMessage(data.to_vec(), ports, shared_memory_regions)))
        }
    }
}

pub struct OsIpcReceiverSet {
    incrementor: RangeFrom<u64>,
    receiver_ids: Vec<u64>,
    receivers: Vec<OsIpcReceiver>,
}

impl OsIpcReceiverSet {
    pub fn new() -> Result<OsIpcReceiverSet, ChannelError> {
        Ok(OsIpcReceiverSet {
            incrementor: 0..,
            receiver_ids: vec![],
            receivers: vec![],
        })
    }

    pub fn add(&mut self, receiver: OsIpcReceiver) -> Result<u64, ChannelError> {
        let last_index = self.incrementor.next().unwrap();
        self.receiver_ids.push(last_index);
        self.receivers.push(receiver.consume());
        Ok(last_index)
    }

    pub fn select(&mut self) -> Result<Vec<OsIpcSelectionResult>, ChannelError> {
        if self.receivers.is_empty() {
            return Err(ChannelError::UnknownError);
        }

        struct Remove(usize, u64);

        // FIXME: Remove early returns and explictly drop `borrows` when lifetimes are non-lexical
        let Remove(r_index, r_id) = {
            let borrows: Vec<_> = self.receivers.iter().map(|r| {
                Ref::map(r.0.borrow(), |o| &o.as_ref().unwrap().receiver)
            }).collect();

            select! {
                recv(borrows.iter().map(|b| &**b), msg, from) => {
                    let r_index = borrows.iter().position(|r| &**r == from).unwrap();
                    let r_id = self.receiver_ids[r_index];
                    if let Some(ChannelMessage(data, channels, shmems)) = msg {
                        let channels = channels.into_iter().map(OsOpaqueIpcChannel::new).collect();
                        return Ok(vec![OsIpcSelectionResult::DataReceived(r_id, data, channels, shmems)])
                    } else {
                        Remove(r_index, r_id)
                    }
                }
            }
        };
        self.receivers.remove(r_index);
        self.receiver_ids.remove(r_index);
        Ok(vec![OsIpcSelectionResult::ChannelClosed(r_id)])
    }
}

pub enum OsIpcSelectionResult {
    DataReceived(u64, Vec<u8>, Vec<OsOpaqueIpcChannel>, Vec<OsIpcSharedMemory>),
    ChannelClosed(u64),
}

impl OsIpcSelectionResult {
    pub fn unwrap(self) -> (u64, Vec<u8>, Vec<OsOpaqueIpcChannel>, Vec<OsIpcSharedMemory>) {
        match self {
            OsIpcSelectionResult::DataReceived(id, data, channels, shared_memory_regions) => {
                (id, data, channels, shared_memory_regions)
            }
            OsIpcSelectionResult::ChannelClosed(id) => {
                panic!("OsIpcSelectionResult::unwrap(): receiver ID {} was closed!", id)
            }
        }
    }
}

pub struct OsIpcOneShotServer {
    receiver: OsIpcReceiver,
    name: String,
}

impl OsIpcOneShotServer {
    pub fn new() -> Result<(OsIpcOneShotServer, String), ChannelError> {
        let (sender, receiver) = try!(channel());

        let name = Uuid::new_v4().to_string();
        let record = ServerRecord::new(sender);
        ONE_SHOT_SERVERS.lock().unwrap().insert(name.clone(), record);
        Ok((OsIpcOneShotServer {
            receiver: receiver,
            name: name.clone(),
        },name.clone()))
    }

    pub fn accept(
        self,
    ) -> Result<
        (
            OsIpcReceiver,
            Vec<u8>,
            Vec<OsOpaqueIpcChannel>,
            Vec<OsIpcSharedMemory>,
        ),
        ChannelError,
    > {
        let record = ONE_SHOT_SERVERS
            .lock()
            .unwrap()
            .get(&self.name)
            .unwrap()
            .clone();
        record.accept();
        ONE_SHOT_SERVERS.lock().unwrap().remove(&self.name).unwrap();
        let (data, channels, shmems) = try!(self.receiver.recv());
        Ok((self.receiver, data, channels, shmems))
    }
}

#[derive(PartialEq, Debug)]
pub enum OsIpcChannel {
    Sender(OsIpcSender),
    Receiver(OsIpcReceiver),
}

#[derive(PartialEq, Debug)]
pub struct OsOpaqueIpcChannel {
    channel: RefCell<Option<OsIpcChannel>>,
}

impl OsOpaqueIpcChannel {
    fn new(channel: OsIpcChannel) -> OsOpaqueIpcChannel {
        OsOpaqueIpcChannel {
            channel: RefCell::new(Some(channel))
        }
    }

    pub fn to_receiver(&self) -> OsIpcReceiver {
        match self.channel.borrow_mut().take().unwrap() {
            OsIpcChannel::Sender(_) => panic!("Opaque channel is not a receiver!"),
            OsIpcChannel::Receiver(r) => r
        }
    }

    pub fn to_sender(&mut self) -> OsIpcSender {
        match self.channel.borrow_mut().take().unwrap() {
            OsIpcChannel::Sender(s) => s,
            OsIpcChannel::Receiver(_) => panic!("Opaque channel is not a sender!"),
        }
    }
}

pub struct OsIpcSharedMemory {
    ptr: *mut u8,
    length: usize,
    data: Arc<Vec<u8>>,
}

unsafe impl Send for OsIpcSharedMemory {}
unsafe impl Sync for OsIpcSharedMemory {}

impl Clone for OsIpcSharedMemory {
    fn clone(&self) -> OsIpcSharedMemory {
        OsIpcSharedMemory {
            ptr: self.ptr,
            length: self.length,
            data: self.data.clone(),
        }
    }
}

impl PartialEq for OsIpcSharedMemory {
    fn eq(&self, other: &OsIpcSharedMemory) -> bool {
        **self == **other
    }
}

impl Debug for OsIpcSharedMemory {
    fn fmt(&self, formatter: &mut Formatter) -> Result<(), fmt::Error> {
        (**self).fmt(formatter)
    }
}

impl Deref for OsIpcSharedMemory {
    type Target = [u8];

    #[inline]
    fn deref(&self) -> &[u8] {
        if self.ptr.is_null() {
            panic!("attempted to access a consumed `OsIpcSharedMemory`")
        }
        unsafe {
            slice::from_raw_parts(self.ptr, self.length)
        }
    }
}

impl OsIpcSharedMemory {
    pub fn from_byte(byte: u8, length: usize) -> OsIpcSharedMemory {
        let mut v = Arc::new(vec![byte; length]);
        OsIpcSharedMemory {
            ptr: Arc::get_mut(&mut v).unwrap().as_mut_ptr(),
            length: length,
            data: v
        }
    }

    pub fn from_bytes(bytes: &[u8]) -> OsIpcSharedMemory {
        let mut v = Arc::new(bytes.to_vec());
        OsIpcSharedMemory {
            ptr: Arc::get_mut(&mut v).unwrap().as_mut_ptr(),
            length: v.len(),
            data: v
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum ChannelError {
    ChannelClosedError,
    BrokenPipeError,
    UnknownError,
}

impl ChannelError {
    #[allow(dead_code)]
    pub fn channel_is_closed(&self) -> bool {
        *self == ChannelError::ChannelClosedError
    }
}

impl From<ChannelError> for bincode::Error {
    fn from(crossbeam_error: ChannelError) -> Self {
        Error::from(crossbeam_error).into()
    }
}

impl From<ChannelError> for Error {
    fn from(crossbeam_error: ChannelError) -> Error {
        match crossbeam_error {
            ChannelError::ChannelClosedError => {
                Error::new(ErrorKind::ConnectionReset, "crossbeam-channel sender closed")
            }
            ChannelError::BrokenPipeError => {
                Error::new(ErrorKind::BrokenPipe, "crossbeam-channel receiver closed")
            }
            ChannelError::UnknownError => {
                Error::new(ErrorKind::Other, "Other crossbeam-channel error")
            }
        }
    }
}

