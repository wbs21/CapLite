use std::ffi::{c_void, c_int};
use std::ptr;
use muxide::Mp4Writer;
use std::fs::File;
use std::fmt;
use tracing::{info, warn, error};

extern "C" {
    pub fn WelsCreateSVCEncoder(ppEncoder: *mut *mut c_void) -> c_int;
    pub fn WelsDestroySVCEncoder(pEncoder: *mut c_void);
    pub fn WelsSetEncoderParam(pEncoder: *mut c_void, pParam: *const c_void) -> c_int;
    pub fn WelsEncodeFrame(pEncoder: *mut c_void, pSrc: *const c_void, pDst: *mut u8, pDstLen: *mut c_int) -> c_int;
}

#[repr(C)]
#[derive(Debug)]
pub struct SEncParamBase {
    pub iPicWidth: c_int,
    pub iPicHeight: c_int,
    pub iTargetBitrate: c_int,
    pub iMaxBitrate: c_int,
    pub iRCMode: c_int,
    pub iTemporalLayerNum: c_int,
    pub iSpatialLayerNum: c_int,
    pub bEnableFrameSkip: c_int,
    pub iMaxSpatialLayerNum: c_int,
    pub sSpatialLayers: [SSpatialLayerConfig;4],
}
#[repr(C)]
#[derive(Debug, Default)]
pub struct SSpatialLayerConfig {
    pub iVideoWidth: c_int,
    pub iVideoHeight: c_int,
    pub fFrameRate: f32,
    pub iSpatialBitrate: c_int,
    pub iMaxSpatialBitrate: c_int,
}

#[derive(Debug)]
pub enum AvError {
    EncoderCreateFail(i32),
    EncoderConfigFail(i32),
    LibYuvConvertFail(i32),
    EncodeFail(i32),
    IoError(std::io::Error),
    MuxError(muxide::MuxError),
    NoSpsPps,
    NullPointer,
}
impl fmt::Display for AvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AvError::EncoderCreateFail(r) => write!(f, "OpenH264 创建失败 ret={r}"),
            AvError::EncoderConfigFail(r) => write!(f, "OpenH264 参数配置失败 ret={r}"),
            AvError::LibYuvConvertFail(r) => write!(f, "libyuv BGRA转换I420失败 ret={r}"),
            AvError::EncodeFail(r) => write!(f, "编码帧失败 ret={r}"),
            AvError::IoError(e) => write!(f, "IO: {e}"),
            AvError::MuxError(e) => write!(f, "MP4封装: {e}"),
            AvError::NoSpsPps => write!(f, "未获取SPS/PPS"),
            AvError::NullPointer => write!(f, "空指针/非法参数"),
        }
    }
}
impl std::error::Error for AvError {}
impl From<std::io::Error> for AvError {
    fn from(e: std::io::Error) -> Self { AvError::IoError(e) }
}
impl From<muxide::MuxError> for AvError {
    fn from(e: muxide::MuxError) -> Self { AvError::MuxError(e) }
}

extern "C" {
    fn ARGBToI420(
        src_bgra: *const u8,
        src_stride: c_int,
        dst_y: *mut u8,
        dst_y_stride: c_int,
        dst_u: *mut u8,
        dst_u_stride: c_int,
        dst_v: *mut u8,
        dst_v_stride: c_int,
        width: c_int,
        height: c_int,
    ) -> c_int;
}

pub struct AvRecorder {
    encoder: *mut c_void,
    mp4_writer: Option<Mp4Writer<File>>,
    width: i32,
    height: i32,
    fps: u32,
    timestamp: u64,
    i420_y: Vec<u8>,
    i420_u: Vec<u8>,
    i420_v: Vec<u8>,
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
    is_started: bool,
}

impl AvRecorder {
    pub fn new(out_path: &str, width: i32, height: i32, fps: u32, bitrate: i32) -> Result<Self, AvError> {
        if width <=0 || height <=0 || fps ==0 || bitrate <=0 { return Err(AvError::NullPointer); }
        let mut encoder: *mut c_void = ptr::null_mut();
        let ret = unsafe{ WelsCreateSVCEncoder(&mut encoder) };
        if ret !=0 || encoder.is_null() { return Err(AvError::EncoderCreateFail(ret)); }
        let param = SEncParamBase{
            iPicWidth:width, iPicHeight:height, iTargetBitrate:bitrate, iMaxBitrate:bitrate,
            iRCMode:1, iTemporalLayerNum:1, iSpatialLayerNum:1, bEnableFrameSkip:0, iMaxSpatialLayerNum:1,
            sSpatialLayers:[
                SSpatialLayerConfig{iVideoWidth:width,iVideoHeight:height,fFrameRate:fps as f32,iSpatialBitrate:bitrate,iMaxSpatialBitrate:bitrate},
                SSpatialLayerConfig::default(),SSpatialLayerConfig::default(),SSpatialLayerConfig::default()
            ]
        };
        let ret_cfg = unsafe{ WelsSetEncoderParam(encoder, &param as *const _ as *const c_void) };
        if ret_cfg !=0 { unsafe{ WelsDestroySVCEncoder(encoder); } return Err(AvError::EncoderConfigFail(ret_cfg)); }
        let y_size = (width*height) as usize;
        let uv_size = ((width/2)*(height/2)) as usize;
        info!("OpenH264编码器初始化成功 {}x{} fps={}", width, height, fps);
        Ok(Self{
            encoder, mp4_writer:None, width, height, fps, timestamp:0,
            i420_y:vec![0;y_size], i420_u:vec![0;uv_size], i420_v:vec![0;uv_size],
            sps:None,pps:None,is_started:false
        })
    }

    pub fn feed_bgra_frame(&mut self, bgra_data:&[u8]) -> Result<(),AvError> {
        let w = self.width; let h = self.height;
        let expected = (w*h*4) as usize;
        if bgra_data.len() < expected { return Err(AvError::NullPointer); }
        unsafe{
            let r = ARGBToI420(
                bgra_data.as_ptr(), w*4,
                self.i420_y.as_mut_ptr(),w,
                self.i420_u.as_mut_ptr(),w/2,
                self.i420_v.as_mut_ptr(),w/2,
                w,h
            );
            if r !=0 { return Err(AvError::LibYuvConvertFail(r)); }
        }
        let mut nalu_buf = vec![0u8; (w*h) as usize];
        let mut nalu_len:i32 = nalu_buf.len() as i32;
        let ret_enc = unsafe{ WelsEncodeFrame(
            self.encoder, self.i420_y.as_ptr() as *const c_void,
            nalu_buf.as_mut_ptr(), &mut nalu_len
        )};
        if ret_enc !=0 || nalu_len <=0 { return Err(AvError::EncodeFail(ret_enc)); }
        let nalu_data = &nalu_buf[0..nalu_len as usize];
        if !self.is_started {
            self.extract_sps_pps(nalu_data)?;
            let file = File::create(out_path)?;
            let mux = Mp4Writer::new_with_sps_pps(file, self.width as u32, self.height as u32, self.fps, self.sps.as_ref().unwrap(), self.pps.as_ref().unwrap())?;
            self.mp4_writer = Some(mux);
            self.is_started = true;
            info!("MP4 muxer初始化完成");
        }
        if let Some(mux) = &mut self.mp4_writer {
            mux.write_h264_frame(nalu_data, self.timestamp, false)?;
        }
        self.timestamp += 1000/self.fps as u64;
        Ok(())
    }

    fn extract_sps_pps(&mut self, nalu:&[u8]) -> Result<(),AvError> {
        let start_code = &[0x00,0x00,0x01];
        let mut pos = 0;
        while pos < nalu.len() {
            if let Some(p) = nalu[pos..].windows(4).position(|x| x == start_code) {
                let start = pos + p +4;
                let end = nalu[start..].windows(4).position(|x|x==start_code).map_or(nalu.len(), |v| start+v);
                let slice = &nalu[start..end];
                let nalu_type = slice[0] &0x1F;
                match nalu_type {
                    0x07 => self.sps = Some(slice.to_vec()),
                    0x08 => self.pps = Some(slice.to_vec()),
                    _ => {}
                }
                pos = end;
            } else { break; }
        }
        if self.sps.is_none() || self.pps.is_none() { return Err(AvError::NoSpsPps); }
        Ok(())
    }

    pub fn stop(&mut self) -> Result<(),AvError> {
        info!("AvRecorder stop");
        if let Some(mut mux) = self.mp4_writer.take() { mux.finish()?; }
        if !self.encoder.is_null() { unsafe{ WelsDestroySVCEncoder(self.encoder); } self.encoder = ptr::null_mut(); }
        self.is_started = false;
        Ok(())
    }
}
impl Drop for AvRecorder {
    fn drop(&mut self) { let _ = self.stop(); }
}
