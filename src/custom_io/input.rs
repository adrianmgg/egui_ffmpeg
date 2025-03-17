
// stolen shamelessly from ffmpeg-the-third/format/context/input.rs

use super::Input;
use ffmpeg_the_third::{codec::packet::Packet, ffi::{av_read_frame, av_read_pause, av_read_play, avformat_seek_file, AVInputFormat}, format, packet::Mut as _, util::range::Range, Error, Stream};


// I may at some point come up with a prettier way of doing what I'm trying to do here.
// The problem is that ffmpeg_the_third::format::Input has a destructor which does things that I do
// not want done, like invoking ffmpeg's function to close a file handle that it opened.
//
// I very pointedly open the file handle myself and implement my own custom read callback.  ffmpeg
// assuming it knows what my data structure looks like when it goes to close it would have
// disastrous consequences.
//
// Instead of using that struct, I implement my own struct that provides the same functionality,
// which takes a lifetime parameter to ensure that it does not outlive the AVIOContext which
// provides its backend.  It has its own Drop implementation which calls avformat_close() but
// leaves the AVIOContext alone (that gets cleaned up by the CustomIO object when it gets
// destroyed).
//
// Unfortunately, since the interface in ffmpeg_the_third doesn't support just turning a pointer
// into an `Input` with no strings attached, and even if it did I wouldn't hvae any way to shoehorn
// my lifetime back in.  There also isn't a trait interface you can implement and have the
// avformat_*() methods just appear. Maybe I could get on the horn to the ffmpeg-sys-the-third guys
// and see about adding one, but it doesn't seem likely.  For now I'm just reimplementing the Input
// struct by copy-pasting the implementations directly from the ffmpeg-sys-the-third
// implementation.

impl<'a> Input<'a> {
    pub fn format(&self) -> format::Input {
        unsafe { format::Input::wrap((*self.as_ptr()).iformat as *mut AVInputFormat) }
    }

    pub fn probe_score(&self) -> i32 {
        unsafe { (*self.as_ptr()).probe_score }
    }

    pub fn packets<'b>(&'b mut self) -> PacketIter<'b> where 'b: 'a {
        PacketIter::new(self)
    }

    pub fn pause(&mut self) -> Result<(), Error> {
        unsafe {
            match av_read_pause(self.as_mut_ptr()) {
                0 => Ok(()),
                e => Err(Error::from(e)),
            }
        }
    }

    pub fn play(&mut self) -> Result<(), Error> {
        unsafe {
            match av_read_play(self.as_mut_ptr()) {
                0 => Ok(()),
                e => Err(Error::from(e)),
            }
        }
    }

    pub fn seek<R: Range<i64>>(&mut self, ts: i64, range: R) -> Result<(), Error> {
        unsafe {
            match avformat_seek_file(
                self.as_mut_ptr(),
                -1,
                range.start().cloned().unwrap_or(i64::min_value()),
                ts,
                range.end().cloned().unwrap_or(i64::max_value()),
                0,
            ) {
                s if s >= 0 => Ok(()),
                e => Err(Error::from(e)),
            }
        }
    }
}


pub struct PacketIter<'a> {
    context: &'a mut Input<'a>,
}

impl<'a> PacketIter<'a> {
    pub fn new(context: &'a mut Input<'a>) -> PacketIter<'a> {
        PacketIter { context }
    }
}

impl<'a> Iterator for PacketIter<'a> {
    type Item = Result<(Stream<'a>, Packet), Error>;

    fn next(&mut self) -> Option<<Self as Iterator>::Item> {
        let mut packet = Packet::empty();

        match packet.custom_read(self.context) {
            Ok(..) => unsafe {
                Some(Ok((
                    Stream::wrap(std::mem::transmute_copy(&self.context), packet.stream()),
                    packet,
                )))
            },

            Err(Error::Eof) => None,

            Err(e) => Some(Err(e)),
        }
    }
}

trait CustomRead {
    fn custom_read<'a>(&mut self, ctx: &mut Input<'a>) -> Result<(), Error>;
}

impl CustomRead for Packet {
    #[inline]
    fn custom_read<'a>(&mut self, format: &mut Input<'a>) -> Result<(), Error> {
        unsafe {
            match av_read_frame(format.as_mut_ptr(), self.as_mut_ptr()) {
                0 => Ok(()),
                e => Err(Error::from(e)),
            }
        }
    }
}
