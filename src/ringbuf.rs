use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::ops::{Deref,DerefMut};

use ffmpeg_the_third::Frame;

struct RingBufInner<const N: usize> {
    buffer: [Frame; N],
    read_offset: usize,
    write_offset: usize,
    writer_wrapped: bool,
    reader_closed: bool,
    writer_closed: bool,
}

type RingBufGuard<'a, const N: usize> = MutexGuard<'a, RingBufInner<N>>;

pub struct RingBufSlot<'a, const N: usize, const Writable: bool> {
    guard: RingBufGuard<'a,N>,
    slot: usize,
}

impl<'a, const N: usize, const Writable: bool> Deref for RingBufSlot<'a, N, Writable> {
    type Target=Frame;
    fn deref(&self)->&Frame {
        &self.guard.buffer[self.slot]
    }
}

impl<'a, const N: usize> DerefMut for RingBufSlot<'a, N, true> {
    fn deref_mut(&mut self)->&mut Frame {
        &mut self.guard.buffer[self.slot]
    }
}

fn try_get_slot<'a, const N: usize, const write: bool>(mut guard: RingBufGuard<'a,N>) -> Result<RingBufSlot<'a,N,write>, RingBufGuard<'a,N>> {
    let can_advance = (guard.read_offset < guard.write_offset) ^ guard.writer_wrapped ^ write;
    if can_advance {
        let slot = if write {
            let slot = guard.write_offset;
            guard.write_offset += 1;
            if guard.write_offset >= N {
                guard.write_offset = 0;
                guard.writer_wrapped = true;
            }
            slot
        } else {
            let slot = guard.read_offset;
            guard.read_offset += 1;
            if guard.read_offset >= N {
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

struct RingBuf<const N: usize> {
    inner: Mutex<RingBufInner<N>>,
    condvar: Condvar,
}

enum TryAcquireError {
    ChannelClosed,
    NotReady,
}

enum AcquireError {
    ChannelClosed,
}

impl<const N: usize> RingBuf<N> {
    fn read(&self) -> Result<RingBufSlot<'_,N,false>,AcquireError> {
        let mut guard = self.inner.lock().unwrap();
        loop {
            guard = match try_get_slot::<N, false>(guard) {
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

    fn try_read(&self) -> Result<RingBufSlot<'_,N,false>,TryAcquireError> {
        let guard = self.inner.lock().unwrap();

        try_get_slot::<N, false>(guard).map_err(|guard| 
            if guard.writer_closed {TryAcquireError::ChannelClosed} else {TryAcquireError::NotReady}
            )
    }

    fn write(&self) -> Result<RingBufSlot<'_,N,true>,AcquireError> {
        let mut guard = self.inner.lock().unwrap();
        loop {
            if guard.reader_closed {
                return Err(AcquireError::ChannelClosed);
            }
            guard = match try_get_slot::<N, true>(guard) {
                Ok(res) => return Ok(res),
                Err(guard) => self.condvar.wait(guard).unwrap(),
            }
        }
    }

    fn try_write(&self) -> Result<RingBufSlot<'_,N,true>,TryAcquireError> {
        let guard = self.inner.lock().unwrap();
        if guard.reader_closed {
            return Err(TryAcquireError::ChannelClosed);
        }

        try_get_slot::<N, true>(guard).map_err(|_| TryAcquireError::NotReady)
    }

}
