use std::sync::Arc;

use ffmpeg_the_third::frame::Video as VideoFrame;

use crate::{ringbuf::RingBuf, DecodeThreadArgs, SynchronizationInfo};

use pending::Pending;

pub struct VideoPlayerWidget {
    video_queue: Pending<Arc<RingBuf<VideoFrame>>>,
    synchronization_info: Arc<SynchronizationInfo>,
    texture: Option<egui::TextureId>,
}

impl VideoPlayerWidget {
    pub fn new(input_create: impl FnOnce(DecodeThreadArgs) + Send + 'static) -> Self {
        let (video_sender, video_receiver) = Pending::new();
        let synchronization_info: Arc<SynchronizationInfo> = Default::default();
        let sync_info2 = synchronization_info.clone();
        std::thread::spawn(move || input_create(DecodeThreadArgs{synchronization_info, video_sender}));

        Self {
            synchronization_info: sync_info2,
            video_queue: video_receiver,
            texture: None,
        }
    }
    pub fn show(&mut self, ui: &mut egui::Ui) -> egui::Response {
        ui.add(self)
    }
    
    fn draw_transport(&mut self, ui: &mut egui::Ui) {
        
    }
}

impl egui::Widget for &mut VideoPlayerWidget {
    fn ui(self, ui: &mut egui::Ui) -> egui::Response {
        ui.with_layout(egui::Layout::bottom_up(egui::Align::Center), |ui| {
            self.draw_transport(ui);
            if let Some(video) = self.video_queue.try_load() {
                if let Ok(frame) = video.read() {
                    // show the video frame
                } else {
                    // if we get here, video is at eof
                }
            } else {
                // if we get here, video is still loading
                
            }
        }).response
    }
}
