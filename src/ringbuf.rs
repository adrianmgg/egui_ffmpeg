use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::ops::{Deref,DerefMut};

use ffmpeg_the_third::Frame;

#[cfg(debug_assertions)]
#[derive(PartialEq,Eq,Debug,Clone,Copy)]
struct RingBufID(u64);

#[cfg(debug_assertions)]
impl RingBufID {
    fn next() -> Self {
        static ID_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        RingBufID(ID_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
    }
}

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
    condvar: &'a Condvar,
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
        //eprintln!("notifying");
        self.condvar.notify_one();
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
        // run drop code for self.guard, but not for self.
        let mut me = std::mem::ManuallyDrop::new(self);
        unsafe {std::ptr::drop_in_place(&mut me.guard as *mut _)};
    }
}

#[derive(Debug,Clone,Copy)]
enum WrapState {
    MayNotWrap,
    MayWrap,
    HasWrapped,
}

pub struct NonNotifyingSlot<'a,T> {
    guard: RingBufGuard<'a, T>,
    pub slot: usize,
}

impl<'a,T> Deref for NonNotifyingSlot<'a,T> {
    type Target=T;
    fn deref(&self) -> &T {
        &self.guard.buffer[self.slot]
    }
}

#[derive(Clone,Copy)]
pub struct RingBufIndex {
    #[cfg(debug_assertions)] iterator_id: RingBufID,
    cursor: usize,
    wrap_state: WrapState,
}

pub struct ReadIterator<'a, T> {
    #[cfg(debug_assertions)] id: RingBufID,
    guard: RingBufGuard<'a, T>,
    cursor: usize,
    wrap_state: WrapState,
    condvar: &'a Condvar,
}

impl<'a, T> ReadIterator<'a, T> {
    pub fn next(&mut self) -> Option<(RingBufIndex, &T)> {
        if self.cursor == self.guard.write_offset && !matches!(self.wrap_state, WrapState::MayWrap) {
            return None;
        }
        let res = &self.guard.buffer[self.cursor];
        let index = RingBufIndex {
            #[cfg(debug_assertions)] iterator_id: self.id,
            cursor: self.cursor,
            wrap_state: self.wrap_state,
        };
        self.cursor += 1;
        if self.cursor >= self.guard.buffer.len() {
            debug_assert!(matches!(self.wrap_state, WrapState::MayWrap), "{:?}", self.wrap_state);
            self.wrap_state = WrapState::HasWrapped;
            self.cursor = 0;
        }
        Some((index, res))
    }

    pub fn reset_to(&mut self, idx: RingBufIndex) {
        #[cfg(debug_assertions)]
        assert!(idx.iterator_id == self.id, "RingBufIndex may only be consumed by the same ReadIterator that created it");
        self.cursor = idx.cursor;
        self.wrap_state = idx.wrap_state;
    }
    pub fn back(&mut self) -> bool {
        if self.cursor == self.guard.read_offset && !matches!(self.wrap_state, WrapState::HasWrapped) {
            return false;
        }
        if self.cursor == 0 {
            debug_assert!(matches!(self.wrap_state, WrapState::HasWrapped), "{:?}", self.wrap_state);
            self.wrap_state = WrapState::MayWrap;
            self.cursor = self.guard.buffer.len()-1;
        } else {
            self.cursor -= 1;
        }
        true
    }

    pub fn mark(mut self) -> Option<NonNotifyingSlot<'a,T>> {
        if self.guard.read_offset != self.cursor {
            self.condvar.notify_one();
        }
        self.guard.read_offset = self.cursor;
        self.guard.writer_wrapped = matches!(self.wrap_state, WrapState::MayWrap);
        if self.cursor == self.guard.write_offset && !matches!(self.wrap_state, WrapState::MayWrap) {
            return None;
        }
        Some(NonNotifyingSlot {guard: self.guard, slot: self.cursor})
    }

    pub fn _dbg_tmp_get_ptr(&self) -> *const () {
        self as *const Self as *const ()
    }
}


fn try_get_slot<'a, T, const WRITE: bool>(mut guard: RingBufGuard<'a, T>, condvar: &'a Condvar) -> Result<RingBufSlot<'a, T, WRITE>, RingBufGuard<'a, T>> {
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
        Ok(RingBufSlot{guard, slot, caused_wrap, condvar})
    } else {
        Err(guard)
    }
}

pub struct RingBuf<T> {
    inner: Mutex<RingBufInner<T>>,
    read_ready: Condvar,
    write_ready: Condvar,
    debug_name: &'static str,
}

impl<T> RingBuf<T> {
    pub fn new(size: usize, init: impl FnMut()->T, debug_name: &'static str) -> Self {
        Self {
            inner: Mutex::new(RingBufInner::new(size, init)),
            read_ready: Condvar::new(),
            write_ready: Condvar::new(),
            debug_name,
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
            guard = match try_get_slot::<T, false>(guard, &self.write_ready) {
                Ok(res) => return Ok(res),
                Err(guard) => {
                    if guard.writer_closed {
                        return Err(AcquireError::ChannelClosed);
                    } else {
                        eprintln!("{} queue underrun", self.debug_name);
                        self.read_ready.wait(guard).unwrap()
                    }
                },
            }
        }
    }

    pub fn try_read(&self) -> Result<RingBufSlot<'_,T,false>,TryAcquireError> {
        let guard = self.inner.lock().unwrap();

        try_get_slot::<T, false>(guard, &self.write_ready)
            .map_err(|guard| 
                if guard.writer_closed {TryAcquireError::ChannelClosed} else {TryAcquireError::NotReady}
            )
    }

    pub fn read_iter(&self) -> ReadIterator<'_,T> {
        let guard = self.inner.lock().unwrap();
        ReadIterator {
            wrap_state: if guard.writer_wrapped {WrapState::MayWrap} else {WrapState::MayNotWrap},
            cursor: guard.read_offset,
            guard,
            condvar: &self.write_ready,
            #[cfg(debug_assertions)] id: RingBufID::next(),
        }
    }

    pub fn write(&self) -> Result<RingBufSlot<'_,T,true>,AcquireError> {
        let mut guard = self.inner.lock().unwrap();
        loop {
            if guard.reader_closed {
                return Err(AcquireError::ChannelClosed);
            }
            guard = match try_get_slot::<T, true>(guard, &self.read_ready) {
                Ok(res) => return Ok(res),
                Err(guard) => {
                    eprintln!("{} queue overrun", self.debug_name);
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

        try_get_slot::<T, true>(guard, &self.read_ready).map_err(|_| TryAcquireError::NotReady)
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
        let ringbuf = RingBuf::new(1, || 0, "");
        assert!(ringbuf.try_read().is_err());
        *ringbuf.try_write().unwrap() = 1;
        assert!(ringbuf.try_write().is_err());
        assert_eq!(*ringbuf.try_read().unwrap(), 1);
        assert!(ringbuf.try_read().is_err());
        assert!(ringbuf.try_write().is_ok());
    }

    #[test]
    fn test_ringbuf_multi_slot() {
        let ringbuf = RingBuf::new(3, || 0, "");
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
        let ringbuf = RingBuf::new(3,||0, "");
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

    #[test]
    fn test_ringbuf_iter() {
        let ringbuf = RingBuf::new(3,||0, "");
        *ringbuf.try_write().unwrap()=1;
        *ringbuf.try_write().unwrap()=2;
        *ringbuf.try_write().unwrap()=3;
        {
            let guard = ringbuf.inner.lock().unwrap();
            assert_eq!(guard.read_offset, guard.write_offset);
        }

        let mut iter = ringbuf.read_iter();
        assert!(!iter.back());
        assert_eq!(iter.next().map(|x|x.1), Some(&1));
        assert_eq!(iter.next().map(|x|x.1), Some(&2));
        assert_eq!(iter.next().map(|x|x.1), Some(&3));
        assert_eq!(iter.next().map(|x|x.1), None);
        assert!(iter.back());
        assert!(iter.back());
        assert!(iter.back());

        assert_eq!(iter.next().map(|x|x.1), Some(&1));
        assert_eq!(iter.next().map(|x|x.1), Some(&2));
        assert_eq!(iter.next().map(|x|x.1), Some(&3));
        assert_eq!(iter.next().map(|x|x.1), None);
        assert!(iter.back());
        assert!(iter.back());
        assert!(iter.back());
        assert!(!iter.back());

        let ringbuf = RingBuf::new(5,||0, "");
        *ringbuf.try_write().unwrap()=1;
        *ringbuf.try_write().unwrap()=2;

        let mut iter = ringbuf.read_iter();

        assert_eq!(iter.next().map(|x|x.1), Some(&1));
        assert_eq!(iter.next().map(|x|x.1), Some(&2));
        assert_eq!(iter.next().map(|x|x.1), None);
        assert!(iter.back());
        assert!(iter.back());
        assert!(!iter.back());

        let ringbuf = RingBuf::new(3,||0, "");
        ringbuf.try_write().unwrap();
        *ringbuf.try_write().unwrap()=1;
        *ringbuf.try_write().unwrap()=2;
        ringbuf.try_read().unwrap(); // advance the read cursor to a nonzero position.

        let mut iter = ringbuf.read_iter();

        assert_eq!(iter.next().map(|x|x.1), Some(&1));
        assert_eq!(iter.next().map(|x|x.1), Some(&2));
        assert_eq!(iter.next().map(|x|x.1), None);
        assert!(iter.back());
        assert!(iter.back());
        assert!(!iter.back());


        // ensure the read range straddles the wrap point.
        let ringbuf = RingBuf::new(3,||0, "");
        ringbuf.try_write().unwrap();
        ringbuf.try_read().unwrap(); 
        ringbuf.try_write().unwrap();
        ringbuf.try_read().unwrap();
        *ringbuf.try_write().unwrap()=1;
        *ringbuf.try_write().unwrap()=2;

        let mut iter = ringbuf.read_iter();

        assert_eq!(iter.next().map(|x|x.1), Some(&1));
        assert_eq!(iter.next().map(|x|x.1), Some(&2));
        assert_eq!(iter.next().map(|x|x.1), None);
        assert!(iter.back());
        assert!(iter.back());
        assert!(!iter.back());


        let ringbuf = RingBuf::new(5,||0, "");
        ringbuf.try_write().unwrap();
        ringbuf.try_read().unwrap(); 
        ringbuf.try_write().unwrap();
        ringbuf.try_read().unwrap(); 
        ringbuf.try_write().unwrap();
        ringbuf.try_read().unwrap(); 
        *ringbuf.try_write().unwrap()=1;
        *ringbuf.try_write().unwrap()=2;
        *ringbuf.try_write().unwrap()=3;
        *ringbuf.try_write().unwrap()=4;
        *ringbuf.try_write().unwrap()=5;

        {
            let guard = ringbuf.inner.lock().unwrap();
            assert_eq!(*guard.buffer, [3,4,5,1,2]);
        }

        let mut iter = ringbuf.read_iter();

        assert!(!iter.back());
        assert_eq!(iter.next().map(|x|x.1), Some(&1));
        assert_eq!(iter.next().map(|x|x.1), Some(&2));
        assert_eq!(iter.next().map(|x|x.1), Some(&3));
        assert_eq!(iter.next().map(|x|x.1), Some(&4));
        assert_eq!(iter.next().map(|x|x.1), Some(&5));
        assert_eq!(iter.next().map(|x|x.1), None);
        assert!(iter.back());
        assert!(iter.back());
        assert!(iter.back());
        assert!(iter.back());
        assert!(iter.back());
        assert!(!iter.back());
        assert_eq!(iter.next().map(|x|x.1), Some(&1));
        assert_eq!(iter.next().map(|x|x.1), Some(&2));
        assert_eq!(iter.next().map(|x|x.1), Some(&3));
        assert_eq!(iter.next().map(|x|x.1), Some(&4));
        assert_eq!(iter.next().map(|x|x.1), Some(&5));
        assert_eq!(iter.next().map(|x|x.1), None);
        
        let ringbuf = RingBuf::new(5,||0, "");
        let mut iter = ringbuf.read_iter();
        assert!(!iter.back());
        assert_eq!(iter.next().map(|x|x.1), None);
        assert!(!iter.back());
    }
}
