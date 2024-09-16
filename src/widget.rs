use std::{sync::{atomic::Ordering, Arc}, time::Duration};

use egui::{load::SizedTexture, Color32, Pos2, Rect, Rounding, Sense, TextureOptions, Vec2};
use ffmpeg_the_third::frame::Video as VideoFrame;

use crate::{ringbuf::RingBuf, DecodeThreadArgs, CpalStreamArgs, SynchronizationInfo};

use pending::Pending;

use cpal::traits::{HostTrait, DeviceTrait, StreamTrait};

pub struct VideoPlayerWidget {
    video_queue: Pending<Arc<RingBuf<VideoFrame>>>,
    audio_args: Option<oneshot::Receiver<CpalStreamArgs>>,
    synchronization_info: Arc<SynchronizationInfo>,
    texture: Option<egui::TextureHandle>,
    last_pts: Option<i64>,
}

#[derive(thiserror::Error,Debug)]
pub enum StartAudioError {
    #[error("No default audio output device found")]
    NoOutputDevice,
    #[error("Error creating audio output stream: {0}")]
    BuildStreamError(#[from] cpal::BuildStreamError),
}

impl VideoPlayerWidget {
    pub fn new(input_create: impl FnOnce(DecodeThreadArgs) + Send + 'static) -> Self {
        let (video_sender, video_receiver) = Pending::new();
        let (cpal_sender, cpal_receiver) = oneshot::channel();
        let synchronization_info: Arc<SynchronizationInfo> = Default::default();
        synchronization_info.is_playing.store(true, Ordering::Relaxed);
        let sync_info2 = synchronization_info.clone();
        std::thread::spawn(move || input_create(DecodeThreadArgs{video_sender, cpal_sender}));

        Self {
            synchronization_info: sync_info2,
            video_queue: video_receiver,
            texture: None,
            last_pts: None,
            audio_args: Some(cpal_receiver),
        }
    }



    /// return values meaning:
    ///
    /// * `Some(Ok(Some(stream)))` - stream was started successfully, destroy it when you stop the
    ///    stream.
    /// * `Some(Ok(None))` - this video file does not have an audio track.
    /// * `Some(Err(...))` - this video file has an audio track, but an error was encountered
    /// trying to create an audio stream to play it.
    /// * `None` - not ready to create the audio stream yet, please try again next frame.
    pub fn try_start_audio(&mut self) -> Option<Result<Option<cpal::Stream>, StartAudioError>> {
        if let Some(ref rcvr) = self.audio_args {
            match rcvr.try_recv() {
                Ok(args) => {
                    Some(self.start_audio_impl(args).map(Option::Some))
                },
                Err(_e) => {Some(Ok(None))},
            }
        } else {
            panic!("try_start_audio() called after returning Some(...)");
        }
    }

    fn start_audio_impl(&self, args: CpalStreamArgs) -> Result<cpal::Stream,StartAudioError> {
        let sync_info = self.synchronization_info.clone();
        let host = cpal::default_host();
        let device = host.default_output_device().ok_or(StartAudioError::NoOutputDevice)?;
        let mut audio_source = args.audio_source;
        let stream = device.build_output_stream_raw(
            &args.stream_config,
            args.format,
            move |data, info| {
                let data = data.bytes_mut();
                if sync_info.is_playing.load(Ordering::Relaxed) {
                    let (pts, done) = audio_source.output(data);
                    if let Some(pts) = pts {
                        let ts = info.timestamp();
                        // TODO do this calculation properly
                        let delta = ts.playback.duration_since(&ts.callback).expect("playback timestamp should always be after callback timestamp");
                        let delta = dbg!(delta);
                        sync_info.current_pts.store(pts - delta.as_millis() as i64, Ordering::Relaxed);
                    }
                    sync_info.audio_eof.store(done, Ordering::Relaxed);
                } else {
                    audio_source.fill_with_silence(data);
                }
            },
            move |error| {
                eprintln!("Error in audio playback: {}", error);
            },
            None,
        )?;
        self.synchronization_info.audio_major.store(true, Ordering::Relaxed);
        Ok(stream)
    }

    pub fn show(&mut self, ui: &mut egui::Ui) -> egui::Response {
        ui.add(self)
    }
    
    fn draw_playback_controls(&mut self, ui: &mut egui::Ui) {
        let (play_button_resp, play_button_painter) = ui.allocate_painter(Vec2::new(20.0,20.0), Sense::click());
        let is_playing = if play_button_resp.clicked() {
            // fetch_not() returns what the value was _before_ it was inverted, so we must invert
            // it again to get the value after.
            !self.synchronization_info.is_playing.fetch_not(Ordering::Relaxed)
        } else {
            self.synchronization_info.is_playing.load(Ordering::Relaxed)
        };
        // TODO draw actual play and pause buttons.
        if is_playing {
            play_button_painter.rect_filled(Rect::EVERYTHING,Rounding::ZERO,Color32::GREEN);
        } else {
            play_button_painter.rect_filled(Rect::EVERYTHING,Rounding::ZERO,Color32::RED);
        }
    }
}

impl egui::Widget for &mut VideoPlayerWidget {
    fn ui(self, ui: &mut egui::Ui) -> egui::Response {
        ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
            let mut time_to_next_frame = None;
            if let Some(video) = self.video_queue.try_load() {
                let current_pts = self.synchronization_info.current_pts.load(Ordering::Relaxed);
                let mut iter = video.read_iter();

                // we want the last frame with a pts less than or equal to current_pts.
                println!("starting iteration, current_pts {}", current_pts);
                while let Some(frame) = iter.next() {
                    if let Some(pts) = frame.pts() {
                        println!("pts={}", pts);
                        if pts > current_pts {
                            time_to_next_frame = Some(Duration::from_millis((pts - current_pts) as u64));
                            iter.back();
                            break;
                        }
                    }
                }
                iter.back();
                let slot = iter.mark();
                
                if let Some(frame) = slot {
                    if !self.last_pts.is_some_and(|pts| pts == frame.pts().unwrap()) {
                        let pixels = frame.plane::<[u8;4]>(0)
                            .into_iter()
                            .copied()
                            .map(|[r,g,b,a]| Color32::from_rgba_unmultiplied(r,g,b,a))
                            .collect::<Vec<_>>();
                        let image = egui::ColorImage {
                            size: [frame.stride(0) as usize / 4, frame.plane_height(0) as usize],
                            pixels
                        };
                        let options = TextureOptions::LINEAR;
                        if let Some(ref mut texture) = self.texture {
                            texture.set(image, options);
                        } else {
                            self.texture = Some(ui.ctx().load_texture("video", image, options));
                        }
                    }
                    ui.add(egui::Image::new(SizedTexture::from_handle(self.texture.as_ref().unwrap()))
                        .maintain_aspect_ratio(true)
                        .shrink_to_fit()
                    );
                    self.last_pts = frame.pts();
                } else {
                    // if we get here, there are no frames for us to display,
                    // which hopefully means the video is still loading?
                    println!("no available frames");
                }
            } else {
                // if we get here, video is still loading
                
            }
            if self.synchronization_info.is_playing.load(Ordering::Relaxed) {
                if let Some(time) = time_to_next_frame {
                    ui.ctx().request_repaint_after(time);
                } else {
                    println!("unknown repaint time, requesting immediate repaint");
                    ui.ctx().request_repaint(); 
                }
            }
            self.draw_playback_controls(ui);
        }).response
    }
}
