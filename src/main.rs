use std::sync::{atomic::Ordering, Arc};

use egui::CentralPanel;
use ffmpeg_player::{self, ringbuf::RingBuf, widget::VideoPlayerWidget, DecodeThreadArgs, SynchronizationInfo};
use ffmpeg_the_third::frame::Video as VideoFrame;

struct VideoPlayerApp {
    widget: VideoPlayerWidget,
}

impl eframe::App for VideoPlayerApp {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        CentralPanel::default().show(ctx, |ui| {
            self.widget.show(ui);
        });
    }
}

fn main() {
    let f = std::fs::File::open("/home/vsparks/Videos/most fashionable faction.webm").unwrap();
    //let data = std::fs::read("/home/vsparks/Videos/lagtrain.webm").unwrap();
    //let f = std::io::Cursor::new(data);
    let widget = VideoPlayerWidget::new(|args| {
        let mut cio = ffmpeg_player::CustomIO::new_read_seekable(f);
        let mut input = cio.open_input().unwrap();
        ffmpeg_player::video_decode_thread(&mut input, args);
    });

    let app = VideoPlayerApp {widget};

    eframe::run_native("ffmpeg video player", eframe::NativeOptions::default(), Box::new(move |_| Ok(Box::new(app)))).unwrap();

    
}

