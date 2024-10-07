#![feature(seek_stream_len)]
#![feature(slice_as_chunks)]
#![feature(array_chunks)]
#![feature(ptr_metadata)]

use std::io::{Read, Seek};
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ffmpeg_the_third::codec::Id;
use ffmpeg_the_third::format::{Pixel, Sample};
use ffmpeg_the_third::ChannelLayoutMask;
use ffmpeg_the_third::{ffi::av_dump_format, format::context::input, media::Type};

use ffmpeg_the_third::{self as ffmpeg, frame::Video as VideoFrame, frame::Audio as AudioFrame, frame::Frame, codec::decoder::opened::Opened as Decoder};
use image::ImageEncoder as _;

mod custom_io;
pub use custom_io::CustomIO;
pub use cpal;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait as _};

pub mod ringbuf;
use ringbuf::RingBuf;

pub mod widget;

fn equilibrium(format: cpal::SampleFormat) -> &'static [u8] {
    match format {
        cpal::SampleFormat::I8 => bytemuck::bytes_of(&<i8 as cpal::Sample>::EQUILIBRIUM),
        cpal::SampleFormat::I16 => bytemuck::bytes_of(&<i16 as cpal::Sample>::EQUILIBRIUM),
        cpal::SampleFormat::I32 => bytemuck::bytes_of(&<i32 as cpal::Sample>::EQUILIBRIUM),
        cpal::SampleFormat::I64 => bytemuck::bytes_of(&<i64 as cpal::Sample>::EQUILIBRIUM),
        cpal::SampleFormat::U8 => bytemuck::bytes_of(&<u8 as cpal::Sample>::EQUILIBRIUM),
        cpal::SampleFormat::U16 => bytemuck::bytes_of(&<u16 as cpal::Sample>::EQUILIBRIUM),
        cpal::SampleFormat::U32 => bytemuck::bytes_of(&<u32 as cpal::Sample>::EQUILIBRIUM),
        cpal::SampleFormat::U64 => bytemuck::bytes_of(&<u64 as cpal::Sample>::EQUILIBRIUM),
        cpal::SampleFormat::F32 => bytemuck::bytes_of(&<f32 as cpal::Sample>::EQUILIBRIUM),
        cpal::SampleFormat::F64 => bytemuck::bytes_of(&<f64 as cpal::Sample>::EQUILIBRIUM),
        _ => todo!(),
    }
}

fn repeat_chunks<const N: usize>(input: &[u8], output: &mut [u8]) {
    let ([input],[]) = input.as_chunks::<N>() else {unreachable!()};
    let (output, []) = output.as_chunks_mut::<N>() else {panic!("output slice length was not a multiple of {}", N)};
    output.fill(*input)
}

fn repeat_to_fill(input: &[u8], output: &mut [u8]) {
    match input.len() {
        1 => output.fill(input[0]),
        2 => repeat_chunks::<2>(input, output),
        4 => repeat_chunks::<4>(input, output),
        8 => repeat_chunks::<8>(input, output),
        len => panic!("unknown slice length {}", len),
    }
}

pub struct StreamSink<T> {
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
    equilibrium: &'static [u8],
    sample_rate: u32,
}

impl AudioPlayer {
    fn new(queue: Arc<RingBuf<AudioFrame>>, sample_rate: u32, bytes_per_sample: usize, equilibrium: &'static [u8]) -> Self {
        Self {
            queue,
            offset_into_current_slot: 0,
            sample_rate,
            bytes_per_sample,
            equilibrium,
        }
    }

    fn output(&mut self, mut output_buffer: &mut [u8]) -> (Option<i64>, bool) {
        let mut pts = None;
        while !output_buffer.is_empty() {
            match self.queue.read() {
                Ok(frame) => {
                    if pts.is_none() {
                        pts = frame.pts();
                        if let Some(ref mut pts) = pts {
                            if self.offset_into_current_slot != 0 {
                                let samples_into_current_slot =self.offset_into_current_slot / self.bytes_per_sample;
                                let millis_into_current_slot = (samples_into_current_slot as i64) * 1000 / self.sample_rate as i64;
                                *pts += millis_into_current_slot;
                            }
                        }
                    }
                    let total_len = frame.samples() * self.bytes_per_sample;
                    let input_data = &frame.data(0)[self.offset_into_current_slot..total_len];
                    self.offset_into_current_slot=0;
                    if input_data.len() > output_buffer.len() {
                        output_buffer.copy_from_slice(&input_data[..output_buffer.len()]);
                        self.offset_into_current_slot = output_buffer.len();
                        frame.do_not_consume();
                        return (pts, false);
                    }
                    output_buffer[..input_data.len()].copy_from_slice(input_data);
                    output_buffer = &mut output_buffer[input_data.len()..];
                },
                Err(_) => {
                    self.fill_with_silence(output_buffer);
                    return (pts, true);
                },
            }
        }
        (pts, false)
    }

    pub fn fill_with_silence(&self, output_buffer: &mut [u8]) {
        repeat_to_fill(self.equilibrium, output_buffer);
    }
}

#[derive(thiserror::Error, Debug)]
pub enum AudioOutputError {
    #[error("error creating codec")]
    CreatingCodec(ffmpeg::Error),
    #[error("error creating decoder")]
    CreatingDecoder(ffmpeg::Error),
    #[error("error creating resampler")]
    CreatingResampler(ffmpeg::Error),
    #[error("no audio output device")]
    NoAudioOutputDevice,
    #[error("error creating audio output stream")]
    BuildStreamError(#[from] cpal::BuildStreamError),
    #[error("error starting audio output stream")]
    PlayStreamError(#[from] cpal::PlayStreamError),
}

pub struct TimeSinceLastReset {
    epoch: Instant,
    time_since: AtomicU64,
}

impl TimeSinceLastReset {
    pub fn last_reset_instant(&self) -> Instant {
        self.epoch + Duration::from_millis(self.time_since.load(Ordering::Relaxed))
    }
    pub fn millis_since_last_reset(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64 - self.time_since.load(Ordering::Relaxed)
    }
    pub fn reset(&self) {
        self.time_since.store(self.epoch.elapsed().as_millis() as u64, Ordering::Relaxed);
    }
}

impl Default for TimeSinceLastReset {
    fn default() -> Self { Self { epoch: Instant::now(), time_since: AtomicU64::from(0) } }
}

#[derive(Default)]
pub struct SynchronizationInfo {
    is_playing: AtomicBool,
    audio_major: AtomicBool,
    current_pts: AtomicI64,
    audio_eof: AtomicBool,
    last_pts_update: TimeSinceLastReset,
}

fn pump_decoder<F: Deref<Target=ffmpeg::Frame> + DerefMut>(handler: &mut StreamSink<F>) -> Result<usize, ffmpeg::Error> {
    let mut count = 0;
    match &mut handler.processing_step {
        Some((temp_frame, processing_step)) => {
            loop {
                // wait until the decode has completed into the temp_frame to lock the output queue,
                // so the lock is only held during the processing step instead of the processing
                // and decode steps.
                // it also means we never call frame.do_not_consume().
                // this should maybe possibly theoretically improve performance maybe.
                // i'm honestly not sure if it will or not.  probably not measurably.  but it can't hurt.
                match handler.decoder.receive_frame(temp_frame) {
                    Ok(()) => {
                        count += 1;
                        match handler.output_queue.write() {
                            Ok(mut frame) => processing_step(&temp_frame, &mut frame),
                            Err(_) => return Err(ffmpeg::Error::Eof),
                        }
                    },
                    Err(ffmpeg::Error::Other{errno: ffmpeg::error::EAGAIN}) => return Ok(count),
                    Err(e) => return Err(e),
                }
            }
        },
        Option::None => {
            loop {
                match handler.output_queue.write() {
                    Ok(mut frame) => match handler.decoder.receive_frame(&mut frame) {
                        Ok(()) => {count += 1;},
                        Err(ffmpeg::Error::Other{errno: ffmpeg::error::EAGAIN}) => {
                            frame.do_not_consume();
                            return Ok(count)
                        },
                        Err(e) => return Err(e),
                    },
                    Err(ringbuf::AcquireError::ChannelClosed) => return Err(ffmpeg::Error::Eof),
                }
            }
        },
    }
}

pub(crate) struct CpalStreamArgs {
    stream_config: cpal::StreamConfig,
    format: cpal::SampleFormat,
    audio_source: AudioPlayer,
}

/*
pub(crate) fn play_audio(queue_consumer: &mut AudioPlayer, synchronization_info: &SynchronizationInfo, data_out: &mut [u8]) {
    if synchronization_info.is_playing.load(Ordering::Relaxed) {
        let (pts, done) = queue_consumer.output(data_out);
        if let Some(pts) = pts {
            let ts = info.timestamp();
            // TODO do this calculation properly
            let delta = ts.playback.duration_since(&ts.callback).expect("playback timestamp should always be after callback timestamp");
            let delta = dbg!(delta);
            synchronization_info.current_pts.store(pts - delta.as_millis() as i64, Ordering::Relaxed);
        }
        synchronization_info.audio_eof.store(done, Ordering::Relaxed);
    } else {
        repeat_to_fill(queue_consumer.equilibrium, data_out);
    }
}
*/

pub struct CpalParameters {
    format: cpal::SampleFormat,
    rate: u32,
    channels: u16,
}

fn create_audio_output(parameters: ffmpeg::codec::Parameters, output_format: Option<CpalParameters>) -> Result<(StreamSink<AudioFrame>, CpalStreamArgs), AudioOutputError> {
    let audio_ctx = ffmpeg::codec::Context::from_parameters(parameters).map_err(AudioOutputError::CreatingCodec)?;
    let audio_decoder = audio_ctx.decoder().audio().map_err(AudioOutputError::CreatingDecoder)?;
    println!("created audio decoder");

    let cpal_format = output_format.as_ref().map(|x| x.format).unwrap_or(match audio_decoder.format() {
            Sample::None => panic!("unknown sample format"),
            Sample::U8(_) => cpal::SampleFormat::U8,
            Sample::I16(_) => cpal::SampleFormat::I16,
            Sample::I32(_) => cpal::SampleFormat::I32,
            Sample::I64(_) => cpal::SampleFormat::I64,
            Sample::F32(_) => cpal::SampleFormat::F32,
            Sample::F64(_) => cpal::SampleFormat::F64,
    });

    let ffmpeg_format = match cpal_format {
            cpal::SampleFormat::U8 => Sample::U8(Type::Packed),
            cpal::SampleFormat::I16 => Sample::I16(Type::Packed),
            cpal::SampleFormat::I32 => Sample::I32(Type::Packed),
            cpal::SampleFormat::I64 => Sample::I64(Type::Packed),
            cpal::SampleFormat::F32 => Sample::F32(Type::Packed),
            cpal::SampleFormat::F64 => Sample::F64(Type::Packed),
            fmt => unimplemented!("sample format {} is not supported by ffmpeg", fmt),
    };

    use ffmpeg::util::format::sample::Type;
    let (channels, rate) = if let Some(CpalParameters {channels, rate, ..}) = output_format {
        (channels, rate)
    } else {
        (audio_decoder.ch_layout().channels() as u16, audio_decoder.rate())
    };
    
    // we perform this check even if output_format is None (i.e. caller doesn't care) in case
    // planar samples have to be converted to packed.

    let resampler = if 
        ffmpeg_format != audio_decoder.format() ||
        rate != audio_decoder.rate() ||
        channels as u32 != audio_decoder.ch_layout().channels()
    {
        // TODO find a better method than default_for_channels() for this
        Some(audio_decoder
            .resampler2(ffmpeg_format,
                ffmpeg::ChannelLayout::default_for_channels(channels as u32),
                rate)
            .map_err(AudioOutputError::CreatingResampler)?
            )
    } else {
        None
    };


    let bytes_per_sample = match ffmpeg_format {
        Sample::None => 0,
        Sample::U8(_) => 1,
        Sample::I16(_) => 2,
        Sample::I32(_) => 4,
        Sample::I64(_) => 8,
        Sample::F32(_) => 4,
        Sample::F64(_) => 8,
    };

    let channel_layout = resampler.as_ref().map(|resampler| resampler.output().channel_layout).unwrap_or(ChannelLayoutMask::all());

    let queue = Arc::new(RingBuf::new(320, || AudioFrame::new(ffmpeg_format, 16384, channel_layout), "audio"));

    let config = CpalStreamArgs {
        stream_config: cpal::StreamConfig {
            channels,
            sample_rate: cpal::SampleRate(rate),
            buffer_size: cpal::BufferSize::Default,
        },
        format: cpal_format,
        audio_source: AudioPlayer::new(queue.clone(),
            audio_decoder.rate(),
            bytes_per_sample * audio_decoder.ch_layout().channels() as usize,
            equilibrium(cpal_format)
            ),
    };
    println!("audio stream started");
    let processing_step = resampler.map(|mut repacker| Box::new(move |input: &AudioFrame, output: &mut AudioFrame| {
        // operations are done in this order to reduce the probability of overflow while preserving as much precision as possible.
        let input_pts = ((input.pts().unwrap() * repacker.input().rate as i64) / 1000) * repacker.output().rate as i64;
        let next_pts = unsafe {ffmpeg::ffi::swr_next_pts(repacker.as_mut_ptr(), input_pts)};
        let next_pts = ((next_pts / repacker.output().rate as i64) * 1000) / repacker.input().rate as i64;

        // if the `samples` field on the output frame is set to 0 when repacker.run() is invoked,
        // ffmpeg will treat it as an output field, writing as many samples as it can into the
        // buffer and returning how many samples it wrote in that same field.
        // if it is not set to 0, ffmpeg will treat it as an input field and return at most that many samples.
        // since we reuse the same Frame over and over to avoid thrashing the memory allocator, we
        // must manually reset this field so that ffmpeg will set it again instead of erroneously
        // interpreting its previous result as its next argument.
        //
        // C code sucks.
        output.set_samples(0); // ensure that ffmpeg will put the number of samples into this buffer, instead of treating it as an input.
        repacker.run(input, output).expect("resampler failed");
        output.set_pts(Some(next_pts));
    }));

    // bizarre workaround for what i can only assume is a compiler bug
    // TODO report this to the rustc team.
    let func = processing_step.map(|a| {
        let b: Box<dyn FnMut(&AudioFrame, &mut AudioFrame)+'static> = a;
        b
    });

    let processing_step = func.map(|func| (AudioFrame::empty(), func));

    Ok((StreamSink {
        decoder: audio_decoder.0,
        processing_step,
        output_queue: queue,
    }, config))
}

pub struct DecodeThreadArgs {
    pub synchronization_info: Arc<SynchronizationInfo>,
    video_sender: oneshot::Sender<Arc<RingBuf<VideoFrame>>>,
    cpal_sender: oneshot::Sender<CpalStreamArgs>,
}

// TODO don't make this a public API please GOD do not make this a public API
pub fn video_decode_thread(input: &mut ffmpeg::format::context::Input, args: DecodeThreadArgs) {
    //unsafe {av_dump_format(input.as_mut_ptr(), 0, c"<custom stream>".as_ptr(), 0);}
    println!("format name is {}", input.format().name());
    input::dump(&input, 0, Some("<custom stream>"));

    let DecodeThreadArgs {synchronization_info, video_sender, cpal_sender} = args;

    let mut video_machinery = input.streams()
        .best(Type::Video)
        .map(|video_stream| {
        let video_ctx = ffmpeg::codec::Context::from_parameters(video_stream.parameters()).expect("unable to create video context");
        //let video_decoder = video_ctx.decoder().video().expect("unable to create video decoder");
        // FFmpeg's native VP9 decoder does not support transparency
        // so in order to support that we must force it to use libvpx.
        let video_codec = match video_ctx.id() {
            //Id::VP8 => ffmpeg::codec::decoder::find_by_name("libvpx-vp8").expect("unable to locate libvpx-vp8 codec"),
            Id::VP9 => ffmpeg::codec::decoder::find_by_name("libvpx-vp9").expect("unable to locate libvpx-vp9 codec"),
            id => ffmpeg::codec::decoder::find(id).expect("unable to locate video decoder"),
        };
        let video_decoder = video_ctx.decoder().open_as(video_codec).and_then(|x|x.video()).expect("unable to create video decoder");
        //let mut scaler = video_decoder.converter(Pixel::RGBA).expect("unable to create color converter");
        let mut scaler = None;
        let video_machinery = StreamSink {
            decoder: video_decoder.0,
            processing_step: Some((VideoFrame::empty(), Box::new(move |frame_in: &VideoFrame, frame_out| {
                // wait to create the color converter until the video stream starts,
                // in case the pixel format changes from YUV to YUVA.
                let scaler = scaler.get_or_insert_with(|| frame_in.converter(Pixel::RGBA).expect("unable to create color converter"));
                scaler.run(frame_in, frame_out).expect("scaler failed");
                frame_out.set_pts(frame_in.pts());
            }))),
            output_queue: Arc::new(RingBuf::new(40, || VideoFrame::empty(), "video")),
        };
        let _ = video_sender.send(video_machinery.output_queue.clone());
        (video_stream.index(), video_machinery)
        });

    let audio_stream = input.streams().best(Type::Audio);

    let mut audio_machinery: Option<(usize, StreamSink<AudioFrame>)> = audio_stream.as_ref().and_then(|stream| {
        match create_audio_output(stream.parameters(), None) {
            Ok((streamsink, cpalstream)) => {
                let _ = cpal_sender.send(cpalstream);
                Some((stream.index(), streamsink))
            },
            Err(e) => {
                eprintln!("unable to create audio output: {}.  playing video only.", e);
                None
            }
        }
    });


    let mut packet = ffmpeg::Packet::empty();
    loop {
        let mut av_offset = 0i64;
        loop {
            match packet.read(&mut *input) {
                Ok(()) => {},
                Err(ffmpeg::Error::Eof) => break,
                Err(e) => panic!("Error reading packet: {}", e),
            }
            //println!("{}", synchronization_info.current_pts.load(Ordering::Relaxed));
            if let Some((video_stream_idx, ref mut machinery)) = video_machinery {
                if packet.stream() == video_stream_idx {
                    machinery.decoder.send_packet(&packet).expect("error decoding packet");
                    let count = pump_decoder(machinery).expect("error decoding video");
                    if count != 1 {
                        println!("video decoder returned {count} frames from a single packet");
                    }
                    av_offset += count as i64;
                } 
            }
            if let Some((audio_stream_idx, machinery)) = &mut audio_machinery {
                if packet.stream() == *audio_stream_idx {
                    machinery.decoder.send_packet(&packet).expect("error decoding packet");
                    match pump_decoder(machinery) {
                        Ok(count) => {
                            if count != 1 {
                                println!("audio decoder returned {count} frames from a single packet");
                            }
                            av_offset -= count as i64;
                        },
                        Err(ffmpeg::Error::Eof) => {},
                        Err(e) => panic!("error decoding audio: {}", e),
                    }
                }
            }
        }
        input.seek(0,..).expect("error rewinding");
    }
    /*
    if let Some((_, ref mut machinery)) = audio_machinery {
        machinery.decoder.send_eof().unwrap();
        let _ = pump_decoder(machinery);
    }
    if let Some((_, ref mut machinery)) = video_machinery {
        machinery.decoder.send_eof().unwrap();
        let _ = pump_decoder(machinery);
    }
    */
}
