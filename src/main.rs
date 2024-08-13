#![feature(seek_stream_len)]
#![feature(new_uninit)]

use std::io::{Read, Seek};
use std::sync::Arc;
use std::time::Duration;

use ffmpeg_the_third::format::{Pixel, Sample};
use ffmpeg_the_third::ChannelLayoutMask;
use ffmpeg_the_third::{ffi::av_dump_format, format::context::input, media::Type};

use ffmpeg_the_third::{self as ffmpeg, frame::Video as VideoFrame, frame::Audio as AudioFrame, frame::Frame, codec::decoder::opened::Opened as Decoder};
use image::ImageEncoder as _;
mod custom_io;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait as _};

pub mod ringbuf;
use ringbuf::RingBuf;

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

struct StreamSink<T> {
    stream_idx: usize,
    decoder: Decoder,
    //processing_step: Option<(T, Box<dyn FnMut(&T, &mut T)>)>,
    output_queue: Arc<RingBuf<T>>,
}

trait _OptionExt<T> {
    fn is_none_or(&self, f: impl FnOnce(&T)->bool)->bool;
}

impl<T> _OptionExt<T> for Option<T> {
    fn is_none_or(&self, f: impl FnOnce(&T)->bool)->bool{
        match self {
            Option::None => true,
            Some(val) => f(val),
        }
    }
}

struct AudioPlayer {
    queue: Arc<RingBuf<AudioFrame>>,
    offset_into_current_slot: usize,
}

impl AudioPlayer {
    fn new(queue: Arc<RingBuf<AudioFrame>>) -> Self {
        Self {
            queue,
            offset_into_current_slot: 0,
        }
    }

    fn output(&mut self, mut output_buffer: &mut [u8]) -> bool {
        while !output_buffer.is_empty() {
            match self.queue.read() {
                Ok(frame) => {
                    if frame.planes()==0 {
                        continue;
                    }
                    let input_data = &frame.data(0)[self.offset_into_current_slot..];
                    self.offset_into_current_slot=0;
                    if input_data.len() > output_buffer.len() {
                        output_buffer.copy_from_slice(&input_data[..output_buffer.len()]);
                        self.offset_into_current_slot = output_buffer.len();
                        return false;
                    }
                    output_buffer[..input_data.len()].copy_from_slice(input_data);
                    output_buffer = &mut output_buffer[input_data.len()..];
                },
                Err(_) => {
                    // TODO fill the rest of the sample with equilibrium data.
                    return true;
                },
            }
        }
        false
    }
}

fn main() {
    let f = std::fs::File::open("/home/vsparks/Videos/lagtrain.webm").unwrap();
    //let data = std::fs::read("/home/vsparks/Videos/lagtrain.webm").unwrap();
    //let f = std::io::Cursor::new(data);
    let mut cio = custom_io::CustomIO::new_read_seekable(f);
    let mut input = cio.open_input().unwrap();
    unsafe {av_dump_format(input.as_mut_ptr(), 0, c"<custom stream>".as_ptr(), 0);}
    println!("format name is {}", input.format().name());
    input::dump(&input, 0, Some("<custom stream>"));

    let video_stream = input.streams()
        .best(Type::Video)
        .expect("file contains no video stream for us to screenshot");
    let video_stream_idx = video_stream.index();
    let video_ctx = ffmpeg::codec::Context::from_parameters(video_stream.parameters()).expect("unable to create video context");
    let mut video_decoder = video_ctx.decoder().video().expect("unable to create video decoder");
    let mut scaler = video_decoder.converter(Pixel::RGBA).expect("unable to create color converter");

    let audio_stream = input.streams().best(Type::Audio);

    let mut audio_machinery: Option<StreamSink<AudioFrame>> = None;

    #[allow(unused,unused_assignments)]
    let mut audio_output_stream = None;

    if let Some(ffstream) = &audio_stream {
        let audio_ctx = ffmpeg::codec::Context::from_parameters(ffstream.parameters()).expect("unable to create audio context");
        let audio_decoder = audio_ctx.decoder().audio().expect("unable to create audio decoder");
        println!("created audio decoder");
        let host = cpal::default_host();
        if let Some(device) = host.default_output_device() {
            let queue = Arc::new(RingBuf::new(3, || AudioFrame::new(audio_decoder.format(), audio_decoder.frame_size() as usize, ChannelLayoutMask::all())));
            let mut queue_consumer = AudioPlayer::new(queue.clone());
            let queue_closer = queue.clone();
            let res = device.build_output_stream_raw(
                &cpal::StreamConfig {
                    channels: audio_decoder.ch_layout().channels() as u16,
                    sample_rate: cpal::SampleRate(audio_decoder.rate()),
                    buffer_size: cpal::BufferSize::Default,
                },
                match audio_decoder.format() {
                    Sample::None => todo!(),
                    Sample::U8(_) => cpal::SampleFormat::U8,
                    Sample::I16(_) => cpal::SampleFormat::I16,
                    Sample::I32(_) => cpal::SampleFormat::I32,
                    Sample::I64(_) => cpal::SampleFormat::I64,
                    Sample::F32(_) => cpal::SampleFormat::F32,
                    Sample::F64(_) => cpal::SampleFormat::F64,
                },
                move |data, info| {
                    if queue_consumer.output(data.bytes_mut()) {
                        // todo shut down the stream
                    }
                }, 
                move |error| {
                    eprintln!("audio decode error: {}", error);
                    queue_closer.close_read();
                },
                Some(Duration::from_millis(200)),
            );
            match res {
                Ok(stream) => {
                    stream.play().unwrap();
                    println!("audio stream started");
                    audio_machinery = Some(StreamSink {
                        stream_idx: ffstream.index(),
                        decoder: audio_decoder.0,
                        output_queue: queue,
                    });
                    audio_output_stream = Some(stream);

                },
                Err(e) => {
                    eprintln!("Could not build the audio stream! {}", e);
                }
            }
        }
    };

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
                        if frames < 10000 {
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
        } else {
            let mut audio_stop=false;
            if let Some(machinery) = &mut audio_machinery {
                if stream.index() == machinery.stream_idx {
                    machinery.decoder.send_packet(&packet).expect("error decoding packet");
                    let mut count=0;
                    loop {
                        let mut slot = match machinery.output_queue.write() {
                            Ok(x) => x,
                            Err(_) => {
                                audio_stop=true;
                                break;
                            },
                        };
                        match machinery.decoder.receive_frame(&mut *slot) {
                            Ok(()) => {
                                if slot.planes() == 0 {
                                    println!("decoder produced an empty frame");
                                    slot.do_not_consume();
                                } else {
                                    println!("filled slot {}", slot.slot);
                                }
                                count+=1;
                            },
                            Err(ffmpeg::Error::Eof) => {
                                machinery.output_queue.close_write();
                                audio_stop=true;
                            },
                            Err(ffmpeg::Error::Other{errno: ffmpeg::error::EAGAIN}) => {
                                slot.do_not_consume();
                                break;
                            },
                            Err(e) => {
                                machinery.output_queue.close_write();
                                panic!("error decoding audio: {}", e);
                            },
                        }
                    }
                }
            }
            if audio_stop {
                audio_machinery = None;
            }
        }
    }
}
