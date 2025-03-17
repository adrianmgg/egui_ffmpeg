use ffmpeg_the_third::ffi::AVFormatContext;

trait Common {
    fn as_ptr(&self) -> *const AVFormatContext;
    fn as_mut_ptr(&mut self) -> *mut AVFormatContext;
}
