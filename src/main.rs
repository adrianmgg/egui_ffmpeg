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
    processing_step: Option<(T, Box<dyn FnMut(&T, &mut T)>)>,
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
    bytes_per_sample: usize,
}

impl AudioPlayer {
    fn new(queue: Arc<RingBuf<AudioFrame>>, bytes_per_sample: usize) -> Self {
        Self {
            queue,
            offset_into_current_slot: 0,
            bytes_per_sample,
        }
    }

    fn output(&mut self, mut output_buffer: &mut [u8]) -> bool {
        while !output_buffer.is_empty() {
            match self.queue.read() {
                Ok(frame) => {
                    let total_len = frame.samples() * self.bytes_per_sample;
                    let input_data = &frame.data(0)[self.offset_into_current_slot..total_len];
                    self.offset_into_current_slot=0;
                    if input_data.len() > output_buffer.len() {
                        output_buffer.copy_from_slice(&input_data[..output_buffer.len()]);
                        self.offset_into_current_slot = output_buffer.len();
                        frame.do_not_consume();
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
    let mut cio = custom_io::CustomIO::new_read_nonseekable(f);
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
            use ffmpeg::util::format::sample::Type;
            let sample_repack_to = match audio_decoder.format() {
                Sample::U8(Type::Planar) => Some(Sample::U8(Type::Packed)), 
                Sample::I16(Type::Planar) => Some(Sample::I16(Type::Packed)),
                Sample::I32(Type::Planar) => Some(Sample::I32(Type::Packed)),
                Sample::I64(Type::Planar) => Some(Sample::I64(Type::Packed)),
                Sample::F32(Type::Planar) => Some(Sample::F32(Type::Packed)),
                Sample::F64(Type::Planar) => Some(Sample::F64(Type::Packed)),
                _ => None,
            };

            let repacker = sample_repack_to.map(|output_sample| audio_decoder.resampler2(output_sample, audio_decoder.ch_layout(), audio_decoder.rate()).expect("Failed to create audio resampler"));

            let channel_layout = repacker.as_ref().map(|r| r.output().channel_layout).unwrap_or(ChannelLayoutMask::all());


            let queue = Arc::new(RingBuf::new(5, || AudioFrame::new(sample_repack_to.unwrap_or(audio_decoder.format()), 8192, channel_layout)));
            let bytes_per_sample = match audio_decoder.format() {
                Sample::None => 0,
                Sample::U8(_) => 1,
                Sample::I16(_) => 2,
                Sample::I32(_) => 4,
                Sample::I64(_) => 8,
                Sample::F32(_) => 4,
                Sample::F64(_) => 8,
            };
            let mut queue_consumer = AudioPlayer::new(queue.clone(), bytes_per_sample * audio_decoder.ch_layout().channels() as usize);
            let res = device.build_output_stream_raw(
                &cpal::StreamConfig {
                    channels: audio_decoder.ch_layout().channels() as u16,
                    sample_rate: cpal::SampleRate(audio_decoder.rate()),
                    buffer_size: cpal::BufferSize::Default,
                },
                // TODO handle the case where the audio driver can't accept this sample format.
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
                    // errors here are usually non fatal, so we don't close the stream here.
                },
                Some(Duration::from_millis(200)), // arbitrarily chosen
            );
            match res {
                Ok(stream) => {
                    stream.play().unwrap();
                    println!("audio stream started");
                    let processing_step = repacker.map(|mut repacker| Box::new(move |input: &AudioFrame, output: &mut AudioFrame| {
                        // operations are done in this order to reduce the probability of overflow while preserving as much precision as possible.
                        let input_pts = ((input.pts().unwrap() * repacker.input().rate as i64) / 1000) * repacker.output().rate as i64;
                        let next_pts = unsafe {ffmpeg::ffi::swr_next_pts(repacker.as_mut_ptr(), input_pts)};
                        let next_pts = ((next_pts / repacker.output().rate as i64) * 1000) / repacker.input().rate as i64;
                        output.set_samples(0); // ensure that ffmpeg will put the number of samples into this buffer, instead of treating it as an input.
                        let delay = repacker.run(input, output).expect("resampler failed");
                        dbg!(delay);
                        output.set_pts(Some(next_pts));
                        dbg!(input.pts(), output.pts());
                        dbg!(input.samples(), output.samples());
                    }));

                    // bizarre workaround for what i can only assume is a compiler bug
                    // TODO report this to the rustc team.
                    let func = processing_step.map(|a| {
                        let b: Box<dyn FnMut(&AudioFrame, &mut AudioFrame)+'static> = a;
                        b
                    });

                    let processing_step = func.map(|func| (AudioFrame::empty(), func));
                        
                    audio_machinery = Some(StreamSink {
                        stream_idx: ffstream.index(),
                        decoder: audio_decoder.0,
                        processing_step,
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
                        if frames < 100 {
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
                    loop {
                        let mut slot = match machinery.output_queue.write() {
                            Ok(x) => x,
                            Err(_) => {
                                audio_stop=true;
                                break;
                            },
                        };
                        let res = if let Some((frame, _)) = &mut machinery.processing_step {
                            machinery.decoder.receive_frame(frame)
                        } else {
                            machinery.decoder.receive_frame(&mut *slot)
                        };
                        match res {
                            Ok(()) => {
                                if let Some((frame, process)) = &mut machinery.processing_step {
                                    process(frame, &mut *slot);
                                }
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
