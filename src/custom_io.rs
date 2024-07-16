use std::{ffi::{c_int, c_long, c_void}, io::{Read, Seek, SeekFrom, Write}, marker::PhantomData, mem::ManuallyDrop, ptr::addr_of};

use ffmpeg_the_third::ffi::{
    av_freep,
    av_malloc,
    avformat_alloc_context,
    avformat_close_input,
    avformat_find_stream_info,
    avformat_open_input,
    avio_alloc_context,
    avio_context_free,
    AVFormatContext,
    AVIOContext,
    AVERROR_UNKNOWN};

use ffmpeg_the_third::Error;

use ffmpeg_the_third::format::context::Input as FFInput;

pub struct CustomIO<T> {
    avio_ctx: *mut AVIOContext,
    _phantom: std::marker::PhantomData<Box<T>>,
}

macro_rules! custom_io_new {
    ($name: ident, {$($bounds:tt)*}, $writable: expr, $read: expr, $write:expr, $seek:expr) => {
        pub fn $name(fp: T) -> Self where T: $($bounds)* {
            let opaque = Box::into_raw(Box::new(fp)) as *mut c_void;
            unsafe {
                let avio_ctx = avio_alloc_context(alloc_buffer(), BUFFER_SIZE as c_int, $writable, opaque, $read, $write, $seek);
                Self {
                    avio_ctx,
                    _phantom: PhantomData,
                }
            }
        }
    };
}

impl<T> CustomIO<T> {
    custom_io_new!(new_read_seekable, {Read+Seek}, 0, Some(callback_native_read::<T>), None, Some(callback_native_seek::<T>));
    custom_io_new!(new_read_nonseekable, {Read}, 0, Some(callback_native_read::<T>), None, None);
    custom_io_new!(new_write_seekable, {Write+Seek}, 1, None, Some(callback_native_write::<T>), Some(callback_native_seek::<T>));
    custom_io_new!(new_write_nonseekable, {Write}, 1, None, Some(callback_native_write::<T>), None);
    custom_io_new!(new_rw_seekable, {Read+Write+Seek}, 1, Some(callback_native_read::<T>), Some(callback_native_write::<T>), Some(callback_native_seek::<T>));
    custom_io_new!(new_rw_nonseekable, {Read+Write}, 1, Some(callback_native_read::<T>), Some(callback_native_write::<T>), None);

    unsafe fn _destroy(&mut self) -> Box<T> {
        let ptr = (*self.avio_ctx).opaque as *mut T;
        av_freep(addr_of!((*self.avio_ctx).buffer) as *mut c_void);
        avio_context_free(&mut self.avio_ctx);

        let inner = Box::from_raw(ptr);
        inner
    }

    pub fn into_inner(mut self) -> Box<T> {
        // SAFETY: self is never used again and is not dropped.
        let res = unsafe { self._destroy() };
        std::mem::forget(self);
        res
    }

    pub fn open_input(&mut self) -> Result<Input<'_>, Error> {
        unsafe {Input::new(&mut *self.avio_ctx)}
    }

    pub unsafe fn as_mut_ptr(&mut self) -> *mut AVIOContext {
        self.avio_ctx
    }
}

pub struct Input<'a> {
    inner: ManuallyDrop<FFInput>,
    _phantom: PhantomData<&'a ()>,
}

impl<'a> Input<'a> {
    pub fn new(ctx: &'a mut AVIOContext) -> Result<Input<'a>, Error> {
        unsafe {
            let mut ptr = avformat_alloc_context();
            if ptr.is_null() {
                panic!("out of memory");
            }
            (*ptr).pb = ctx;

            let r = avformat_open_input(&mut ptr, std::ptr::null(), std::ptr::null(), std::ptr::null_mut());
            if r < 0 {
                return Err(Error::from(r));
            }
            println!("open_input() success");

            let r = avformat_find_stream_info(ptr, std::ptr::null_mut());
            if r < 0 {
                avformat_close_input(&mut ptr);
                return Err(Error::from(r));
            }

            println!("find_stream_info() success");

            // SAFETY: The constructor of FFInput does not do anything.
            // Its *de*structor, on the other hand, makes assumptions about the contents of the
            // AVIOContext which are false in our case, and will result in undefined behavior if
            // executed.
            // We thus wrap it in a ManuallyDrop so that its destructor never runs, and implement
            // our own destructor which destroys just the data we want it to.
            let inner = FFInput::wrap(ptr);

            Ok(Self {
                inner: ManuallyDrop::new(inner),
                _phantom: PhantomData,
            })
        }
    }

}

impl std::ops::Deref for Input<'_> {
    type Target=FFInput;
    fn deref(&self) -> &FFInput {
        &self.inner
    }
}

impl std::ops::DerefMut for Input<'_> {
    fn deref_mut(&mut self) -> &mut FFInput {
        &mut self.inner
    }
}

impl Drop for Input<'_> {
    fn drop(&mut self) {
        unsafe {
            let mut ptr = self.inner.as_mut_ptr();
            avformat_close_input(&mut ptr);
        }
    }
}

impl<T> Drop for CustomIO<T> {
    fn drop(&mut self) {
        // SAFETY: self is never used again (because it is currently being dropped)
        unsafe { self._destroy(); }
    }
}


///////////////////////////////////////////////////////////////////////////////////////////////////////////
//                                   FFI FUNCTIONS                                                       //
///////////////////////////////////////////////////////////////////////////////////////////////////////////

const BUFFER_SIZE: usize=4096;

fn alloc_buffer() -> *mut u8 {
    unsafe {
        let buf = av_malloc(BUFFER_SIZE) as *mut u8;
        if buf.is_null() {
            panic!("out of memory");
        }
        buf
    }
}

unsafe extern "C" fn callback_native_read<T: Read>(state: *mut c_void, buf: *mut u8, buf_size: c_int) -> c_int {
    let buf = std::slice::from_raw_parts_mut(buf, buf_size.try_into().expect("ffmpeg gave us a negative buffer size"));
    let state = state as *mut T;
    let state = &mut *state;
    let res= state.read(buf);
    println!("ffmpeg asked us to read {} bytes -- our response was {:?}", buf.len(), res);
    match res {
        Ok(0) => ffmpeg_the_third::ffi::AVERROR_EOF,
        Ok(count) => count.try_into().unwrap(),
        Err(e) => e.raw_os_error().map(|e| -e).unwrap_or(AVERROR_UNKNOWN),
    }
}

unsafe extern "C" fn callback_native_write<T: Write>(state: *mut c_void, buf: *const u8, buf_size: c_int) -> c_int {
    let buf = std::slice::from_raw_parts(buf, buf_size.try_into().expect("ffmpeg gave us a negative buffer size"));
    let state = state as *mut T;
    let state = &mut *state;
    match state.write(buf) {
        Ok(count) => count.try_into().unwrap(),
        Err(e) => e.raw_os_error().map(|e| -e).unwrap_or(AVERROR_UNKNOWN),
    }
}


unsafe extern "C" fn callback_native_seek<T: Seek>(state: *mut c_void, offset: c_long, whence: c_int) -> c_long {
    let callback = state as *mut T;
    let callback = &mut *callback;

    let mut lock = std::io::stdout().lock();

    let result = match whence {
        ffmpeg_the_third::ffi::AVSEEK_SIZE => {
            write!(lock, "ffmpeg asked for our stream_len()").unwrap();
            callback.stream_len()
        },
        other => {
            let seek_from = match other {
                0 => SeekFrom::Start(offset as u64),
                1 => SeekFrom::Current(offset),
                2 => SeekFrom::End(offset),
                _ => return AVERROR_UNKNOWN.into(),
            };
            write!(lock, "ffmpeg asked us to seek: {:?}", seek_from).unwrap();
            callback.seek(seek_from)
        }
    };

    write!(lock, " -- our response was {:?}\n", result).unwrap();
    std::mem::drop(lock);

    match result {
        Ok(new_position) => new_position.try_into().unwrap(),
        Err(e) => e.raw_os_error().map(|e| -e).unwrap_or(AVERROR_UNKNOWN).into(),
    }
}

