use std::{sync::{atomic::Ordering, Arc}, time::Duration};

use egui::{load::SizedTexture, Color32, Pos2, Rect, Rounding, Sense, TextureOptions, Vec2};
use ffmpeg_the_third::frame::Video as VideoFrame;

use crate::{ringbuf::RingBuf, DecodeThreadArgs, SynchronizationInfo};

use pending::Pending;

pub struct VideoPlayerWidget {
    video_queue: Pending<Arc<RingBuf<VideoFrame>>>,
    synchronization_info: Arc<SynchronizationInfo>,
    texture: Option<egui::TextureHandle>,
    last_pts: Option<i64>,
}

impl VideoPlayerWidget {
    pub fn new(input_create: impl FnOnce(DecodeThreadArgs) + Send + 'static) -> Self {
        let (video_sender, video_receiver) = Pending::new();
        let synchronization_info: Arc<SynchronizationInfo> = Default::default();
        synchronization_info.is_playing.store(true, Ordering::Relaxed);
        let sync_info2 = synchronization_info.clone();
        std::thread::spawn(move || input_create(DecodeThreadArgs{synchronization_info, video_sender}));

        Self {
            synchronization_info: sync_info2,
            video_queue: video_receiver,
            texture: None,
            last_pts: None,
        }
    }
    pub fn show(&mut self, ui: &mut egui::Ui) -> egui::Response {
        ui.add(self)
    }
    
    fn draw_playback_controls(&mut self, ui: &mut egui::Ui) {
        let (play_button_resp, play_button_painter) = ui.allocate_painter(Vec2::new(20.0,20.0), Sense::click());
        let is_playing = if play_button_resp.clicked() {
            println!("pause clicked");
            !self.synchronization_info.is_playing.fetch_not(Ordering::AcqRel)
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
        ui.with_layout(egui::Layout::bottom_up(egui::Align::Center), |ui| {
            let mut time_to_next_frame = None;
            self.draw_playback_controls(ui);
            if let Some(video) = self.video_queue.try_load() {
                let current_pts = self.synchronization_info.current_pts.load(Ordering::Relaxed);
                let mut iter = video.read_iter();


                // we want the last frame with a pts less than or equal to current_pts.
                while let Some(frame) = iter.next() {
                    if let Some(pts) = frame.pts() {
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
                    if !self.last_pts.is_some_and(|pts| pts != frame.pts().unwrap()) {
                        let pixels = frame.data(0)
                            .array_chunks::<4>()
                            .copied()
                            .map(|[r,g,b,a]| Color32::from_rgba_unmultiplied(r,g,b,a))
                            .collect::<Vec<_>>();
                        let image = egui::ColorImage {
                            size: [frame.width() as usize, frame.height() as usize],
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
                }
            } else {
                // if we get here, video is still loading
                
            }
            if let Some(time) = time_to_next_frame {
                ui.ctx().request_repaint_after(time);
            } else {
                ui.ctx().request_repaint(); 
            }
        }).response
    }
}
