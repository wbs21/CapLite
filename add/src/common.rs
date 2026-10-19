use std::time::Instant;
use winapi::um::processthreadsapi::SetThreadPriority;
use winapi::um::winnt;
use winapi::um::handleapi::CloseHandle;
use winapi::um::processthreadsapi::OpenProcess;
use winapi::shared::winerror;

pub const WASAPI_SAMPLE_RATE: f32 = 48000.0;
pub const WASAPI_CHANNELS: usize = 2;
pub const RINGBUF_SECONDS: f32 = 10.0;
pub const SMALL_RING_SECONDS: f32 = 0.200;
pub const VIDEO_LAG_THRESHOLD: f64 = 0.120;
pub const MIN_FRAG_DURATION_US: &str = "2000000";

#[derive(Debug, Clone)]
pub enum MediaPacket {
    Audio { pts: f64, data: Vec<f32> },
    Video { pts: f64, nalu: Vec<u8>, is_key: bool },
}

pub struct AudioClock {
    sample_counter: u64,
    sample_rate: f64,
}

impl AudioClock {
    pub fn new(sample_rate: f64) -> Self {
        Self {
            sample_counter: 0,
            sample_rate,
        }
    }
    pub fn consume_samples(&mut self, cnt: u64) -> f64 {
        let pts = self.sample_counter as f64 / self.sample_rate;
        self.sample_counter += cnt;
        pts
    }
    pub fn current_pts(&self) -> f64 {
        self.sample_counter as f64 / self.sample_rate
    }
}

pub struct QpcTimer {
    start: Instant,
}

impl QpcTimer {
    pub fn new() -> Self {
        Self { start: Instant::now() }
    }
    pub fn pts_sec(&self) -> f64 {
        self.start.elapsed().as_secs_f64()
    }
}

pub fn set_thread_high() {
    unsafe {
        SetThreadPriority(winapi::um::winapi::GetCurrentThread(), winnt::THREAD_PRIORITY_HIGHEST);
    }
}
pub fn set_thread_above_normal() {
    unsafe {
        SetThreadPriority(winapi::um::winapi::GetCurrentThread(), winnt::THREAD_PRIORITY_ABOVE_NORMAL);
    }
}
pub fn set_thread_below_normal() {
    unsafe {
        SetThreadPriority(winapi::um::winapi::GetCurrentThread(), winnt::THREAD_PRIORITY_BELOW_NORMAL);
    }
}

pub fn set_process_below_normal(pid: u32) -> bool {
    unsafe {
        let handle = OpenProcess(winnt::PROCESS_SET_INFORMATION, 0, pid);
        if handle.is_null() {
            return false;
        }
        let ret = winapi::um::processthreadsapi::SetPriorityClass(handle, winnt::BELOW_NORMAL_PRIORITY_CLASS);
        CloseHandle(handle);
        ret != 0
    }
}
