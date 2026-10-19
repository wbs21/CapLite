use std::process::{Command, Child};
use crate::common::{set_process_below_normal, MIN_FRAG_DURATION_US};
use crate::logger::log_info;

pub fn start_ffmpeg() -> Child {
    let child = Command::new("ffmpeg")
        .args([
            "-y",
            "-f", "mpegts",
            "-i", "pipe:0",
            "-c:v", "copy",
            "-c:a", "aac",
            "-b:a", "128k",
            "-f", "mp4",
            "-movflags", "+empty_moov+dash+faststart",
            "-min_frag_duration", MIN_FRAG_DURATION_US,
            "out.mp4",
        ])
        .spawn()
        .expect("ffmpeg spawn failed");
    let pid = child.id();
    set_process_below_normal(pid);
    log_info(format!("ffmpeg started pid={pid}"));
    child
}
