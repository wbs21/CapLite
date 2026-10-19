use std::sync::Arc;
use std::thread;
use crossbeam_channel::bounded;

use crate::audio_capture::TwoStageAudioRing;
use crate::common::AudioClock;
use crate::audio_consumer::audio_consumer_loop;
use crate::dxgi_capture::dxgi_capture_loop;
use crate::video_consumer::video_encode_loop;
use crate::mux::mux_loop;
use crate::ffmpeg_proc::start_ffmpeg;
use crate::logger::{log_info, log_close};
use crate::wasapi::start_wasapi_loopback;

fn main() {
    log_info("recorder start");

    // Audio
    let two_stage_ring = Arc::new(TwoStageAudioRing::new());
    two_stage_ring.clone().start_transport();
    start_wasapi_loopback(two_stage_ring.clone()).expect("wasapi failed");
    let big_consumer = two_stage_ring.take_big_consumer();
    let audio_clock = AudioClock::new(48000.0);
    let (audio_pes_tx, audio_pes_rx) = bounded(200);

    thread::spawn(move || {
        audio_consumer_loop(big_consumer, audio_clock, audio_pes_tx);
    });

    // Video
    let qpc = crate::common::QpcTimer::new();
    let (frame_tx, frame_rx) = bounded(5);
    let (video_pes_tx, video_pes_rx) = bounded(60);

    thread::spawn(move || {
        dxgi_capture_loop(qpc, frame_tx);
    });
    thread::spawn(move || {
        video_encode_loop(frame_rx, video_pes_tx);
    });

    // Mux + FFmpeg
    let mut ffmpeg = start_ffmpeg();
    let stdin = ffmpeg.stdin.take().unwrap();
    thread::spawn(move || {
        mux_loop(audio_pes_rx, video_pes_rx, stdin);
    });

    // 主线程等待ffmpeg退出
    let _ = ffmpeg.wait();
    log_close();
}
