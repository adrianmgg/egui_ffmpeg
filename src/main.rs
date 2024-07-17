#![feature(seek_stream_len)]

use std::io::{Read, Seek};

use ffmpeg_the_third::format::Pixel;
use ffmpeg_the_third::{ffi::av_dump_format, format::context::input, media::Type};

use ffmpeg_the_third::{self as ffmpeg, frame::Video as VideoFrame};
use image::ImageEncoder as _;
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
    let f = std::fs::File::open("/home/vsparks/Videos/Nimona.webm").unwrap();
    //let data = std::fs::read("/home/vsparks/Videos/lagtrain.webm").unwrap();
    //let f = std::io::Cursor::new(data);
    let mut cio = custom_io::CustomIO::new_read_seekable(f);
    let mut input = cio.open_input().unwrap();
//    unsafe {av_dump_format(input.as_mut_ptr(), 0, b"<custom stream>\0".as_ptr() as *const _, 0);}
    println!("format name is {}", input.format().name());
    input::dump(&input, 0, Some("<custom stream>"));

    let video_stream = input.streams()
        .best(Type::Video)
        .expect("file contains no video stream for us to screenshot");
    let video_stream_idx = video_stream.index();
    let video_ctx = ffmpeg::codec::Context::from_parameters(video_stream.parameters()).expect("unable to open video stream");
    println!("video context established");
    let mut video_decoder = video_ctx.decoder().video().expect("unable to create video decoder");
    println!("video decoder ready");
    let mut scaler = video_decoder.converter(Pixel::RGBA).expect("unable to create color converter");

    let mut frame = VideoFrame::empty();
    let mut converted_frame = VideoFrame::empty();

    let mut frames=0;

    for packet in input.packets() {
        let (stream, packet) = packet.expect("error reading packet");
        if stream.index() == video_stream_idx {
            video_decoder.send_packet(&packet).expect("error decoding packet");
            loop {
                match video_decoder.receive_frame(&mut frame) {
                    Ok(()) => {
                        frames += 1;
                        if frames < 1000 {
                            continue;
                        }
                        scaler.run(&frame, &mut converted_frame).expect("error converting frame");
                        let out = std::fs::File::create("output.png").expect("unable to open output file");
                        let encoder = image::codecs::png::PngEncoder::new(out);
                        encoder.write_image(converted_frame.data(0), converted_frame.width(), converted_frame.height(), image::ExtendedColorType::Rgba8).expect("error encoding PNG");
                        return;
                    }
                    Err(ffmpeg::Error::Eof) | Err(ffmpeg::Error::Other{errno: ffmpeg::error::EAGAIN}) => break,
                    Err(e) => panic!("error decoding frame: {}", e),
                }
            }
        }
    }

}
