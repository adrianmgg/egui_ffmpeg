use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::ops::{Deref,DerefMut};

use ffmpeg_the_third::Frame;

struct RingBufInner<T> {
    buffer: Box<[T]>,
    read_offset: usize,
    write_offset: usize,
    writer_wrapped: bool,
    reader_closed: bool,
    writer_closed: bool,
}

impl<T> RingBufInner <T>{
    fn new(size: usize, mut init: impl FnMut()->T) -> Self {
        // an unsafe method call and TWO unstable feature flags.
        // there has GOT to be a better way to initialize an array with multiple function calls!
        //
        // TODO badger the Rust developers to make this part of the standard library.
        let mut buffer = Box::new_uninit_slice(size);
        for e in buffer.iter_mut() {
            e.write(init());
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

type RingBufGuard<'a,T> = MutexGuard<'a, RingBufInner<T>>;

pub struct RingBufSlot<'a, T, const Writable: bool> {
    guard: RingBufGuard<'a, T>,
    pub slot: usize,
    caused_wrap: bool,
    condvar: Option<&'a Condvar>,
}

impl<'a, T, const Writable: bool> Deref for RingBufSlot<'a, T, Writable> {
    type Target=T;
    fn deref(&self)->&T {
        &self.guard.buffer[self.slot]
    }
}

impl<'a, T> DerefMut for RingBufSlot<'a, T, true> {
    fn deref_mut(&mut self)->&mut T {
        &mut self.guard.buffer[self.slot]
    }
}

impl<'a, T> RingBufSlot<'a, T,true> {
    /*
    pub fn pts_sift_down(&mut self) {
        // TODO finish this method.
        let mut i = self.slot;
        
        // SAFETY: the following code cannot panic.
        loop {
            let prev_idx = if i == 0 && self.guard.writer_wrapped {
                self.guard.buffer.len()-1
            } else {i-1};
            if prev_idx == self.guard.read_offset {break;}
            
        }
    }
    */
}

impl<'a, T, const WRITE: bool> Drop for RingBufSlot<'a,T,WRITE> {
    fn drop(&mut self) {
        if let Some(condvar) = self.condvar {
            condvar.notify_one();
        }
    }
}

impl<'a, T, const WRITE: bool> RingBufSlot<'a,T,WRITE> {

    /**
     * Inform the ring buffer that the calling code did not do anything with this slot.  This same
     * slot will be returned again the next time read() or write() is called.
     */
    pub fn do_not_consume(mut self) {
        if WRITE {
            self.guard.write_offset = self.slot;
            if self.caused_wrap {
                debug_assert!(self.guard.writer_wrapped);
                self.guard.writer_wrapped=false;
            }
        } else {
            self.guard.read_offset = self.slot;
            if self.caused_wrap {
                debug_assert!(!self.guard.writer_wrapped);
                self.guard.writer_wrapped=true;
            }
        }
        self.condvar=None;
    }
}

fn try_get_slot<'a, T, const WRITE: bool>(mut guard: RingBufGuard<'a, T>) -> Result<RingBufSlot<'a, T, WRITE>, RingBufGuard<'a, T>> {
    let can_advance = guard.read_offset != guard.write_offset || (WRITE ^ guard.writer_wrapped);
    if can_advance {
        let mut caused_wrap=false;
        let slot = if WRITE {
            let slot = guard.write_offset;
            guard.write_offset += 1;
            if guard.write_offset >= guard.buffer.len() {
                guard.write_offset = 0;
                guard.writer_wrapped = true;
                caused_wrap=true;
            }
            slot
        } else {
            let slot = guard.read_offset;
            guard.read_offset += 1;
            if guard.read_offset >= guard.buffer.len() {
                guard.read_offset = 0;
                guard.writer_wrapped = false;
                caused_wrap=true;
            }
            slot
        };
        Ok(RingBufSlot{guard, slot, caused_wrap, condvar:None})
    } else {
        Err(guard)
    }
}

pub struct RingBuf<T> {
    inner: Mutex<RingBufInner<T>>,
    read_ready: Condvar,
    write_ready: Condvar,
}

impl<T> RingBuf<T> {
    pub fn new(size: usize, init: impl FnMut()->T) -> Self {
        Self {
            inner: Mutex::new(RingBufInner::new(size, init)),
            read_ready: Condvar::new(),
            write_ready: Condvar::new(),
        }
    }
}

#[derive(Debug)]
pub enum TryAcquireError {
    ChannelClosed,
    NotReady,
}

#[derive(Debug)]
pub enum AcquireError {
    ChannelClosed,
}

impl<T> RingBuf<T> {
    pub fn read(&self) -> Result<RingBufSlot<'_,T,false>,AcquireError> {
        let mut guard = self.inner.lock().unwrap();
        loop {
            guard = match try_get_slot::<T, false>(guard) {
                Ok(mut res) => {
                    res.condvar = Some(&self.write_ready);
                    return Ok(res)
                },
                Err(guard) => {
                    if guard.writer_closed {
                        return Err(AcquireError::ChannelClosed);
                    } else {
                        eprintln!("queue underrun");
                        self.read_ready.wait(guard).unwrap()
                    }
                },
            }
        }
    }

    pub fn try_read(&self) -> Result<RingBufSlot<'_,T,false>,TryAcquireError> {
        let guard = self.inner.lock().unwrap();

        let r = try_get_slot::<T, false>(guard).map_err(|guard| 
            if guard.writer_closed {TryAcquireError::ChannelClosed} else {TryAcquireError::NotReady}
            );
        r.map(|mut x| {
            x.condvar = Some(&self.write_ready);
            x
            })
    }

    pub fn write(&self) -> Result<RingBufSlot<'_,T,true>,AcquireError> {
        let mut guard = self.inner.lock().unwrap();
        loop {
            if guard.reader_closed {
                return Err(AcquireError::ChannelClosed);
            }
            guard = match try_get_slot::<T, true>(guard) {
                Ok(mut res) => {
                    res.condvar = Some(&self.read_ready);
                    return Ok(res)
                },
                Err(guard) => {
                    eprintln!("queue overrun");
                    self.write_ready.wait(guard).unwrap()
                },
            }
        }
    }

    pub fn try_write(&self) -> Result<RingBufSlot<'_,T,true>,TryAcquireError> {
        let guard = self.inner.lock().unwrap();
        if guard.reader_closed {
            return Err(TryAcquireError::ChannelClosed);
        }

        let r = try_get_slot(guard).map_err(|_| TryAcquireError::NotReady);
        r.map(|mut x| {
            x.condvar = Some(&self.read_ready);
            x
        })
    }

    pub fn close_read(&self) {
        self.inner.lock().unwrap().reader_closed = true;
    }
    pub fn close_write(&self) {
        self.inner.lock().unwrap().writer_closed = true;
    }
}

#[cfg(test)]
mod test {
    use super::*;
    #[test]
    fn test_ringbuf_one_slot() {
        let ringbuf = RingBuf::new(1, || 0);
        assert!(ringbuf.try_read().is_err());
        *ringbuf.try_write().unwrap() = 1;
        assert!(ringbuf.try_write().is_err());
        assert_eq!(*ringbuf.try_read().unwrap(), 1);
        assert!(ringbuf.try_read().is_err());
        assert!(ringbuf.try_write().is_ok());
    }

    #[test]
    fn test_ringbuf_multi_slot() {
        let ringbuf = RingBuf::new(3, || 0);
        assert!(ringbuf.try_read().is_err());
        *ringbuf.try_write().unwrap() = 1;
        *ringbuf.try_write().unwrap() = 2;
        let mut s = ringbuf.try_write().unwrap();
        *s = 2;
        s.do_not_consume();
        *ringbuf.try_write().unwrap() = 3;
        assert!(ringbuf.try_write().is_err());
        assert_eq!(*ringbuf.try_read().unwrap(), 1);
        assert_eq!(*ringbuf.try_read().unwrap(), 2);
        let mut s = ringbuf.try_write().unwrap();
        *s = 999;
        s.do_not_consume();
        *ringbuf.try_write().unwrap() = 4;
        *ringbuf.try_write().unwrap() = 5;
        assert_eq!(*ringbuf.try_read().unwrap(), 3);
        let s = ringbuf.try_read().unwrap();
        assert_eq!(*s, 4);
        s.do_not_consume();
        assert_eq!(*ringbuf.try_read().unwrap(), 4);
        assert_eq!(*ringbuf.try_read().unwrap(), 5);
        assert!(ringbuf.try_read().is_err());
    }
    
    #[test]
    fn test_ringbuf_end() {
        let ringbuf = RingBuf::new(3,||0);
        assert!(ringbuf.try_write().is_ok());
        assert!(ringbuf.try_read().is_ok());
        assert!(ringbuf.try_write().is_ok());
        assert!(ringbuf.try_read().is_ok());
        ringbuf.try_write().unwrap().do_not_consume();
        assert!(ringbuf.try_read().is_err());

        
        assert!(ringbuf.try_write().is_ok());
        assert!(ringbuf.try_write().is_ok());
        assert!(ringbuf.try_write().is_ok());
        ringbuf.try_read().unwrap().do_not_consume();
        assert!(ringbuf.try_write().is_err());
    }
}
