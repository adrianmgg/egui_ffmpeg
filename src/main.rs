use std::sync::Arc;

use ffmpeg_player::{self, ringbuf::RingBuf};
use ffmpeg_the_third::frame::Video as VideoFrame;

enum Pending<T> {
    Ready(T),
    Pending(oneshot::Receiver<T>),
}

impl<T> Pending<T> {
    fn load(&mut self) -> Option<&mut T> {
        replace_with::replace_with_or_abort(self, |me| {
            match me {
                Pending::Ready(val) => Pending::Ready(val),
                Pending::Pending(rcvr) => match rcvr.try_recv() {
                    Ok(val) => Pending::Ready(val),
                    Err(_) => Pending::Pending(rcvr),
                }
            }
        });
        match self {
            Pending::Ready(val) => Some(val),
            Pending::Pending(_) => None,
        }
    }
}



struct VideoPlayerApp {
    video_queue: Pending<Arc<RingBuf<VideoFrame>>>,
}

impl eframe::App for VideoPlayerApp {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        todo!()
    }
}

fn main() {
    let f = std::fs::File::open("/home/vsparks/Videos/most fashionable faction.webm").unwrap();
    //let data = std::fs::read("/home/vsparks/Videos/lagtrain.webm").unwrap();
    //let f = std::io::Cursor::new(data);
    let (video_sender, video_receiver) = oneshot::channel();
    let synchronization_info: Arc<ffmpeg_player::SynchronizationInfo> = Default::default();
    let sync_info2 = synchronization_info.clone();
    std::thread::spawn(move || {
        let mut cio = ffmpeg_player::CustomIO::new_read_seekable(f);
        let mut input = cio.open_input().unwrap();
        ffmpeg_player::video_decode_thread(&mut input, sync_info2, video_sender);
    });

    
}

