use crate::av_backend::{AvRecorder, AvError};
use crate::config::CaptureRegion;
use crate::wgc_capture::WgcCapture;
use crate::frame_ring::FrameRing;
use std::sync::{Arc,Mutex};
use std::thread;
use std::time::{Instant, Duration};
use tracing::{info, warn, error};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecorderState { Idle, Recording, Stopping }

pub struct ScreenRecorder {
    state: Arc<Mutex<RecorderState>>,
    av_recorder: Option<AvRecorder>,
    capture_region: Option<CaptureRegion>,
    fps: u32,
    bitrate: i32,
    output_path: String,
    capture_thread: Option<thread::JoinHandle<()>>,
    encode_thread: Option<thread::JoinHandle<()>>,
    ring: FrameRing,
    start_time: Arc<Mutex<Option<Instant>>>,
}
impl ScreenRecorder {
    pub fn new(output_path:String, fps:u32, bitrate:i32) -> Self {
        Self {
            state:Arc::new(Mutex::new(RecorderState::Idle)),
            av_recorder:None,
            capture_region:None,
            fps,
            bitrate,
            output_path,
            capture_thread:None,
            encode_thread:None,
            ring: FrameRing::new(20),
            start_time: Arc::new(Mutex::new(None)),
        }
    }
    pub fn set_capture_region(&mut self, region:CaptureRegion) { self.capture_region = Some(region); }

    pub fn get_record_duration(&self) -> Duration {
        let lock = self.start_time.lock().unwrap();
        match *lock {
            Some(t) => t.elapsed(),
            None => Duration::ZERO,
        }
    }

    pub fn start(&mut self) -> Result<(), AvError> {
        let mut state = self.state.lock().unwrap();
        if *state != RecorderState::Idle { return Err(AvError::EncodeFail(-1)); }
        let region = self.capture_region.as_ref().ok_or(AvError::NullPointer)?;

        let mut w = region.width as i32;
        let mut h = region.height as i32;
        if w % 2 != 0 { w -=1; }
        if h % 2 !=0 { h -=1; }
        if w <=0 || h <=0 { return Err(AvError::NullPointer); }

        let mut av = AvRecorder::new(&self.output_path, w, h, self.fps, self.bitrate)?;
        self.av_recorder = Some(av);
        *state = RecorderState::Recording;
        *self.start_time.lock().unwrap() = Some(Instant::now());
        drop(state);
        info!("开始录制，选区：{:?}", region);
        info!("视频分辨率：{}x{}，fps={}, bitrate={}", w, h, self.fps, self.bitrate);

        let state_clone = Arc::clone(&self.state);
        let region_clone = region.clone();
        let fps = self.fps;
        let mut ring = std::mem::take(&mut self.ring);

        // 编码线程
        let encode_handle = thread::spawn(move || {
            let mut av_thread = self.av_recorder.take().unwrap();
            loop {
                let st = *state_clone.lock().unwrap();
                if st != RecorderState::Recording { break; }
                match ring.pop_frame() {
                    Some(bgra) => {
                        if let Err(e) = av_thread.feed_bgra_frame(&bgra) {
                            error!("编码错误: {e}");
                            break;
                        }
                    }
                    None => continue,
                }
            }
            ring.clear();
            if let Err(e) = av_thread.stop() { error!("停止释放av错误 {e}"); }
            *state_clone.lock().unwrap() = RecorderState::Idle;
            *self.start_time.lock().unwrap() = None;
        });
        self.encode_thread = Some(encode_handle);

        // 采集线程
        let capture_state = Arc::clone(&self.state);
        let capture_handle = thread::spawn(move || {
            let wgc = match WgcCapture::new(region_clone) {
                Ok(v)=>v,
                Err(e)=>{error!("WGC启动失败 {e}");return;}
            };
            let frame_interval = std::time::Duration::from_secs_f64(1.0 / fps as f64);
            let mut last = Instant::now();
            loop {
                let st = *capture_state.lock().unwrap();
                if st != RecorderState::Recording { break; }
                let now = Instant::now();
                if now - last >= frame_interval {
                    last = now;
                    match wgc.next_frame() {
                        Ok(Some(bgra)) => {
                            ring.push_frame(bgra);
                        }
                        Ok(None)=>{},
                        Err(e)=>{error!("采集帧错误 {e}");break;}
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        });
        self.capture_thread = Some(capture_handle);
        Ok(())
    }
    pub fn stop(&mut self) -> Result<(), AvError> {
        info!("停止录制");
        let mut state = self.state.lock().unwrap();
        if *state != RecorderState::Recording { return Ok(()); }
        *state = RecorderState::Stopping;
        drop(state);
        if let Some(h) = self.capture_thread.take() { let _ = h.join(); }
        if let Some(h) = self.encode_thread.take() { let _ = h.join(); }
        Ok(())
    }
    pub fn get_state(&self) -> RecorderState { *self.state.lock().unwrap() }
}

pub fn format_duration(dur: Duration) -> String {
    let total_sec = dur.as_secs();
    let h = total_sec / 3600;
    let m = (total_sec % 3600) / 60;
    let s = total_sec % 60;
    format!("{h:02}:{m:02}:{s:02}")
}
