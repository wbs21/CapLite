use ringbuf::RingBuffer;
use std::sync::Arc;
use std::thread;

use crate::common::{set_thread_high, WASAPI_SAMPLE_RATE, WASAPI_CHANNELS, SMALL_RING_SECONDS, RINGBUF_SECONDS};

pub struct TwoStageAudioRing {
    small_ring: RingBuffer<f32>,
    big_ring: RingBuffer<f32>,
}

impl TwoStageAudioRing {
    pub fn new() -> Self {
        let small_size = (SMALL_RING_SECONDS * WASAPI_SAMPLE_RATE * WASAPI_CHANNELS as f32) as usize;
        let big_size = (RINGBUF_SECONDS * WASAPI_SAMPLE_RATE * WASAPI_CHANNELS as f32) as usize;
        Self {
            small_ring: RingBuffer::new(small_size),
            big_ring: RingBuffer::new(big_size),
        }
    }

    /// WASAPI回调入口，仅写入小ring
    pub fn wasapi_callback(&self, pcm: &[f32]) {
        let mut w = self.small_ring.write_producer();
        let avail = w.slots();
        let copy_len = avail.min(pcm.len());
        w.push_slice(&pcm[0..copy_len]);
    }

    /// 启动搬运线程：小ring -> 大ring
    pub fn start_transport(self: Arc<Self>) {
        thread::spawn(move || {
            set_thread_high();
            let mut r_small = self.small_ring.read_consumer();
            let mut w_big = self.big_ring.write_producer();
            loop {
                while r_small.is_empty() {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                let chunk = r_small.read_chunk();
                let avail_write = w_big.slots();
                let take = chunk.len().min(avail_write);
                w_big.push_slice(&chunk[0..take]);
                r_small.skip(take);
            }
        });
    }

    /// 获取大ring读取端，交给audio consumer
    pub fn take_big_consumer(&self) -> ringbuf::ReadConsumer<f32> {
        self.big_ring.read_consumer()
    }
}
