use egui::CentralPanel;
use ffmpeg_player::{self, widget::VideoPlayerWidget};

struct VideoPlayerApp {
    widget: VideoPlayerWidget,
    stream: Option<Option<cpal::Stream>>,
}

impl eframe::App for VideoPlayerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        CentralPanel::default().show(ctx, |ui| {
            self.widget.show(ui);
            if self.stream.is_none() {
                let stream = self.widget.try_start_audio();
                self.stream = stream.map(|a| a.ok().and_then(|x|x));
            }
        });
    }
}

fn main() {
    //let f = std::fs::File::open("/home/vsparks/Videos/wall-e/Wall-E Disc 1_t00.mkv").unwrap();
    let f = std::fs::File::open(std::env::args_os().nth(1).unwrap()).unwrap();
    //let data = std::fs::read("/home/vsparks/Videos/lagtrain.webm").unwrap();
    //let f = std::io::Cursor::new(data);
    let widget = VideoPlayerWidget::new(|args| {
        let mut cio = ffmpeg_player::CustomIO::new_read_seekable(f);
        let mut input = cio.open_input().unwrap();
        ffmpeg_player::video_decode_thread(&mut input, args);
    });

    let app = VideoPlayerApp {widget, stream: None};

    eframe::run_native("ffmpeg video player", eframe::NativeOptions::default(), Box::new(move |_| Box::new(app))).unwrap();
}
