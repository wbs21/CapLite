use crossbeam_channel::Receiver;
use mpeg2ts::ts::{TsPacket, Pid};
use mpeg2ts::pes::Pes;
use mpeg2ts::psi::{Pat, Pmt, StreamType};
use std::collections::VecDeque;
use std::io::Write;
use std::thread;
use std::time::Duration;

use crate::common::{MediaPacket, AudioClock, VIDEO_LAG_THRESHOLD};
use crate::logger::{log_warn};

const PID_AUDIO: Pid = Pid::new(0x100);
const PID_VIDEO: Pid = Pid::new(0x101);
const PID_PAT: Pid = Pid::new(0);
const PID_PMT: Pid = Pid::new(0x10);

pub fn mux_loop(
    audio_pes_rx: Receiver<MediaPacket>,
    video_pes_rx: Receiver<MediaPacket>,
    mut ffmpeg_stdin: std::process::ChildStdin,
) {
    let mut audio_buf: VecDeque<MediaPacket> = VecDeque::new();
    let mut video_buf: VecDeque<MediaPacket> = VecDeque::new();
    let mut audio_clock = AudioClock::new(48000.0);
    let mut pat_pmt_counter = 0u32;
    const PAT_PMT_INTERVAL: u32 = 30;

    loop {
        while let Ok(pkt) = audio_pes_rx.try_recv() {
            audio_buf.push_back(pkt);
        }
        while let Ok(pkt) = video_pes_rx.try_recv() {
            video_buf.push_back(pkt);
        }

        let now_audio_pts = audio_clock.current_pts();

        while let Some(MediaPacket::Video{pts,..}) = video_buf.front() {
            if *pts < now_audio_pts - VIDEO_LAG_THRESHOLD {
                video_buf.pop_front();
                log_warn("drop lag video packet");
            } else {
                break;
            }
        }

        let front_audio = audio_buf.front();
        let front_video = video_buf.front();

        match (front_audio, front_video) {
            (Some(a_pkt), Some(v_pkt)) => {
                let a_pts = match a_pkt { MediaPacket::Audio{pts,..} => *pts, _=> unreachable!() };
                let v_pts = match v_pkt { MediaPacket::Video{pts,..} => *pts, _=> unreachable!() };
                if a_pts <= v_pts {
                    let pkt = audio_buf.pop_front().unwrap();
                    write_ts_pes(&mut ffmpeg_stdin, &pkt);
                } else {
                    let pkt = video_buf.pop_front().unwrap();
                    write_ts_pes(&mut ffmpeg_stdin, &pkt);
                }
                pat_pmt_counter += 1;
            }
            (Some(_), None) => {
                let pkt = audio_buf.pop_front().unwrap();
                write_ts_pes(&mut ffmpeg_stdin, &pkt);
                pat_pmt_counter +=1;
            }
            (None, Some(v_pkt)) => {
                let MediaPacket::Video{pts,..} = v_pkt else { unreachable!() };
                if *pts < now_audio_pts - VIDEO_LAG_THRESHOLD {
                    video_buf.pop_front();
                    log_warn("drop lag video pkt");
                    continue;
                }
                let pkt = video_buf.pop_front().unwrap();
                write_ts_pes(&mut ffmpeg_stdin, &pkt);
                pat_pmt_counter +=1;
            }
            (None, None) => {
                thread::sleep(Duration::from_millis(1));
                continue;
            }
        }

        if pat_pmt_counter >= PAT_PMT_INTERVAL {
            let pat_pmt_bytes = generate_pat_pmt();
            let _ = ffmpeg_stdin.write_all(&pat_pmt_bytes);
            pat_pmt_counter = 0;
        }
    }
}

fn write_ts_pes(stdin: &mut std::process::ChildStdin, pkt: &MediaPacket) {
    let (pid, payload, pts_sec) = match pkt {
        MediaPacket::Audio { pts, data } => {
            let bytes: Vec<u8> = data.iter().flat_map(|f| f.to_le_bytes()).collect();
            (PID_AUDIO, bytes, *pts)
        }
        MediaPacket::Video { pts, nalu,.. } => {
            (PID_VIDEO, nalu.clone(), *pts)
        }
    };
    let pts = (pts_sec * 90000.0) as u64;
    let pes = Pes::new(pid, payload).with_pts(pts);
    let ts_packets = pes.to_ts_packets();
    for ts in ts_packets {
        let _ = stdin.write_all(ts.as_bytes());
    }
}

fn generate_pat_pmt() -> Vec<u8> {
    let mut out = Vec::new();
    let pat = Pat::new().with_program(1, PID_PMT);
    for pkt in pat.to_ts_packets() {
        out.extend_from_slice(pkt.as_bytes());
    }
    let mut pmt = Pmt::new(PID_PMT);
    pmt.add_stream(PID_AUDIO, StreamType::LPCM);
    pmt.add_stream(PID_VIDEO, StreamType::H264);
    for pkt in pmt.to_ts_packets() {
        out.extend_from_slice(pkt.as_bytes());
    }
    out
}
