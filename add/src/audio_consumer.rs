use crossbeam_channel::Sender;
use std::thread;

use crate::common::{AudioClock, MediaPacket, set_thread_above_normal};
use crate::logger::log_warn;

pub fn audio_consumer_loop(
    mut big_consumer: ringbuf::ReadConsumer<f32>,
    mut audio_clock: AudioClock,
    audio_pes_tx: Sender<MediaPacket>,
) {
    set_thread_above_normal();
    let mut buf = Vec::with_capacity(960);
    loop {
        while big_consumer.is_empty() {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let chunk = big_consumer.read_chunk();
        buf.clear();
        buf.extend_from_slice(chunk);
        let sample_count = buf.len() as u64;
        let pts = audio_clock.consume_samples(sample_count);
        let pkt = MediaPacket::Audio { pts, data: buf.clone() };
        if audio_pes_tx.try_send(pkt).is_err() {
            log_warn("audio pes queue full, drop audio packet");
        }
        big_consumer.skip(chunk.len());
    }
}
