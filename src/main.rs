use std::sync::{atomic::Ordering, Arc};

use ffmpeg_player::{self, ringbuf::RingBuf, DecodeThreadArgs, SynchronizationInfo};
use ffmpeg_the_third::frame::Video as VideoFrame;

fn main() {
    let f = std::fs::File::open("/home/vsparks/Videos/most fashionable faction.webm").unwrap();
    //let data = std::fs::read("/home/vsparks/Videos/lagtrain.webm").unwrap();
    //let f = std::io::Cursor::new(data);
    let (video_sender, video_receiver) = oneshot::channel();
    let synchronization_info: Arc<ffmpeg_player::SynchronizationInfo> = Default::default();
    let sync_info2 = synchronization_info.clone();
    synchronization_info.is_playing.store(true, Ordering::Relaxed);
//    std::thread::spawn(move || {
        let mut cio = ffmpeg_player::CustomIO::new_read_seekable(f);
        let mut input = cio.open_input().unwrap();
        ffmpeg_player::video_decode_thread(&mut input, DecodeThreadArgs {synchronization_info: sync_info2, video_sender});
 //   });

    
}

