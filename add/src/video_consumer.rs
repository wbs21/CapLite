use openh264::encoder::Encoder;
use crossbeam_channel::Sender;
use std::thread;

use crate::common::{CapturedVideoFrame, MediaPacket, set_thread_below_normal};
use crate::video_util::bgra_to_i420;
use crate::logger::log_warn;

pub fn video_encode_loop(
    frame_rx: crossbeam_channel::Receiver<CapturedVideoFrame>,
    video_pes_tx: Sender<MediaPacket>,
) {
    set_thread_below_normal();
    let mut enc = Encoder::new();
    let mut i420_buf = Vec::new();

    while let Ok(frame) = frame_rx.recv() {
        bgra_to_i420(&frame.bgra_data, frame.width, frame.height, &mut i420_buf);
        let nals = enc.encode(&i420_buf, frame.width, frame.height);
        for nalu in nals {
            let pkt = MediaPacket::Video {
                pts: frame.pts,
                nalu: nalu.to_vec(),
                is_key: nalu.is_keyframe(),
            };
            if video_pes_tx.try_send(pkt).is_err() {
                log_warn("video pes queue full, drop nalu");
            }
        }
    }
}
