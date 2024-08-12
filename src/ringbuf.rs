use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::ops::{Deref,DerefMut};

use ffmpeg_the_third::Frame;

struct RingBufInner {
    buffer: Box<[Frame]>,
    read_offset: usize,
    write_offset: usize,
    writer_wrapped: bool,
    reader_closed: bool,
    writer_closed: bool,
}

impl RingBufInner {
    fn new(size: usize) -> Self {
        // an unsafe method call and TWO unstable feature flags.
        // there has GOT to be a better way to initialize an array with multiple function calls!
        //
        // TODO badger the Rust developers to make this part of the standard library.
        let mut buffer = Box::new_uninit_slice(size);
        for e in buffer.iter_mut() {
            e.write(unsafe {Frame::empty()});
        }
        Self {
            buffer: unsafe {buffer.assume_init()},
            read_offset: 0,
            write_offset: 0,
            writer_wrapped: false,
            reader_closed: false,
            writer_closed: false,
        }
    }
}

type RingBufGuard<'a> = MutexGuard<'a, RingBufInner>;

pub struct RingBufSlot<'a, const Writable: bool> {
    guard: RingBufGuard<'a>,
    slot: usize,
}

impl<'a, const Writable: bool> Deref for RingBufSlot<'a, Writable> {
    type Target=Frame;
    fn deref(&self)->&Frame {
        &self.guard.buffer[self.slot]
    }
}

impl<'a> DerefMut for RingBufSlot<'a, true> {
    fn deref_mut(&mut self)->&mut Frame {
        &mut self.guard.buffer[self.slot]
    }
}

impl<'a> RingBufSlot<'a,true> {
}

fn try_get_slot<'a, const write: bool>(mut guard: RingBufGuard<'a>) -> Result<RingBufSlot<'a,write>, RingBufGuard<'a>> {
    let can_advance = guard.read_offset != guard.write_offset || (write ^ guard.writer_wrapped);
    dbg!(guard.read_offset, guard.write_offset, guard.writer_wrapped, write, can_advance);
    if can_advance {
        let slot = if write {
            let slot = guard.write_offset;
            guard.write_offset += 1;
            if guard.write_offset >= guard.buffer.len() {
                guard.write_offset = 0;
                guard.writer_wrapped = true;
            }
            slot
        } else {
            let slot = guard.read_offset;
            guard.read_offset += 1;
            if guard.read_offset >= guard.buffer.len() {
                guard.read_offset = 0;
                guard.writer_wrapped = false;
            }
            slot
        };
        Ok(RingBufSlot{guard, slot})
    } else {
        Err(guard)
    }
}

pub struct RingBuf {
    inner: Mutex<RingBufInner>,
    condvar: Condvar,
}

impl RingBuf {
    fn new(size: usize) -> Self {
        Self {
            inner: Mutex::new(RingBufInner::new(size)),
            condvar: Condvar::new(),
        }
    }
}

pub enum TryAcquireError {
    ChannelClosed,
    NotReady,
}

pub enum AcquireError {
    ChannelClosed,
}

impl RingBuf {
    pub fn read(&self) -> Result<RingBufSlot<'_,false>,AcquireError> {
        let mut guard = self.inner.lock().unwrap();
        loop {
            guard = match try_get_slot::<false>(guard) {
                Ok(res) => return Ok(res),
                Err(guard) => {
                    if guard.writer_closed {
                        return Err(AcquireError::ChannelClosed);
                    } else {
                        self.condvar.wait(guard).unwrap()
                    }
                },
            }
        }
    }

    pub fn try_read(&self) -> Result<RingBufSlot<'_,false>,TryAcquireError> {
        let guard = self.inner.lock().unwrap();

        try_get_slot::<false>(guard).map_err(|guard| 
            if guard.writer_closed {TryAcquireError::ChannelClosed} else {TryAcquireError::NotReady}
            )
    }

    pub fn write(&self) -> Result<RingBufSlot<'_,true>,AcquireError> {
        let mut guard = self.inner.lock().unwrap();
        loop {
            if guard.reader_closed {
                return Err(AcquireError::ChannelClosed);
            }
            guard = match try_get_slot::<true>(guard) {
                Ok(res) => return Ok(res),
                Err(guard) => self.condvar.wait(guard).unwrap(),
            }
        }
    }

    pub fn try_write(&self) -> Result<RingBufSlot<'_,true>,TryAcquireError> {
        let guard = self.inner.lock().unwrap();
        if guard.reader_closed {
            return Err(TryAcquireError::ChannelClosed);
        }

        try_get_slot::<true>(guard).map_err(|_| TryAcquireError::NotReady)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    #[test]
    fn test_ringbuf_one_slot() {
        let ringbuf = RingBuf::new(1);
        assert!(ringbuf.try_read().is_err());
        assert!(ringbuf.try_write().is_ok());
        assert!(ringbuf.try_write().is_err());
        assert!(ringbuf.try_read().is_ok());
        assert!(ringbuf.try_read().is_err());
        assert!(ringbuf.try_write().is_ok());
    }

    #[test]
    fn test_ringbuf_multi_slot() {
        let ringbuf = RingBuf::new(3);
        assert!(ringbuf.try_read().is_err());
        assert!(ringbuf.try_write().is_ok());
        assert!(ringbuf.try_write().is_ok());
        assert!(ringbuf.try_write().is_ok());
        assert!(ringbuf.try_write().is_err());
        assert!(ringbuf.try_read().is_ok());
        assert!(ringbuf.try_read().is_ok());
        assert!(ringbuf.try_write().is_ok());
        assert!(ringbuf.try_write().is_ok());
        assert!(ringbuf.try_read().is_ok());
        assert!(ringbuf.try_read().is_ok());
        assert!(ringbuf.try_read().is_ok());
        assert!(ringbuf.try_read().is_err());
    }
}
