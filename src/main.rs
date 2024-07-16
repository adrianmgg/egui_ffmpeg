#![feature(seek_stream_len)]

use std::io::{Read, Seek};

use ffmpeg_the_third::{ffi::av_dump_format, format::context::input};
mod custom_io;

#[allow(dead_code)]
struct BadReadWrapper<T>(T);

impl<T> Read for BadReadWrapper<T> where T: Read {
    fn read(&mut self, mut buf: &mut [u8]) -> std::io::Result<usize> {
        let l = buf.len();
        if l > 50 {
            buf = &mut buf[..50];
        }
        self.0.read(buf)
    }
}

impl<T:Seek> Seek for BadReadWrapper<T> {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        self.0.seek(pos)
    }
    fn stream_len(&mut self) -> std::io::Result<u64> {
        self.0.stream_len()
    }
}

fn main() {
    //let f = std::fs::File::open("/home/vsparks/Videos/lagtrain.webm").unwrap();
    let data = std::fs::read("/home/vsparks/Videos/lagtrain.webm").unwrap();
    let f = std::io::Cursor::new(data);
    let mut cio = custom_io::CustomIO::new_read_seekable(f);
    let mut input = cio.open_input().unwrap();
//    unsafe {av_dump_format(input.as_mut_ptr(), 0, b"<custom stream>\0".as_ptr() as *const _, 0);}
    println!("format name is {}", input.format().name());
    input::dump(&input, 0, Some("<custom stream>"));
}
