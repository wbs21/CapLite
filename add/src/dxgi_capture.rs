use crossbeam_channel::Sender;
use std::mem;
use std::thread;
use winapi::um::dxgi::*;
use winapi::um::dxgitype::*;
use winapi::um::winuser::*;
use winapi::shared::winerror::*;

use crate::common::QpcTimer;
use crate::logger::{log_info, log_warn, log_error};

#[derive(Debug, Clone)]
pub struct CapturedVideoFrame {
    pub bgra_data: Vec<u8>,
    pub width: usize,
    pub height: usize,
    pub pts: f64,
}

pub fn dxgi_capture_loop(qpc: QpcTimer, frame_tx: Sender<CapturedVideoFrame>) {
    unsafe {
        CoInitializeEx(ptr::null_mut(), COINIT_APARTMENTTHREADED).ok();
    }

    let mut dxgi_factory: *mut IDXGIFactory1 = ptr::null_mut();
    unsafe {
        CreateDXGIFactory1(&IID_IDXGIFactory1, &mut dxgi_factory as *mut _)?;
    }
    let factory = dxgi_factory;

    let mut adapter: *mut IDXGIAdapter1 = ptr::null_mut();
    unsafe {
        (*factory).EnumAdapters1(0, &mut adapter as *mut _)?;
    }
    let adapter = adapter;

    let mut output: *mut IDXGIOutput = ptr::null_mut();
    unsafe {
        (*adapter).EnumOutputs(0, &mut output as *mut _)?;
    }
    let output = output;

    let output1: *mut IDXGIOutput1 = unsafe {
        let mut o1 = ptr::null_mut();
        (*output).QueryInterface(&IID_IDXGIOutput1, &mut o1 as *mut _)?;
        o1
    };

    let mut dup: *mut IDXGIOutputDuplication = ptr::null_mut();
    unsafe {
        (*output1).DuplicateOutput(GetDesktopWindow(), &mut dup as *mut _)?;
    }
    let dup = dup;
    log_info("DXGI Desktop Duplication started");

    let mut frame_info: DXGI_OUTDUPL_FRAME_INFO = unsafe { mem::zeroed() };
    let mut resource: *mut IDXGIResource = ptr::null_mut();

    loop {
        let hr = unsafe {
            (*dup).AcquireNextFrame(100, &mut frame_info, &mut resource as *mut _)
        };
        if hr == DXGI_ERROR_WAIT_TIMEOUT {
            continue;
        }
        if hr != S_OK {
            log_warn("DXGI acquire frame failed");
            break;
        }

        let surface: *mut IDXGISurface = unsafe {
            let mut s = ptr::null_mut();
            (*resource).QueryInterface(&IID_IDXGISurface, &mut s as *mut _)?;
            s
        };

        let mut map: DXGI_MAPPED_RECT = unsafe { mem::zeroed() };
        unsafe { (*surface).Map(&mut map, DXGI_MAP_READ)?; }
        let desc = unsafe {
            let mut d: DXGI_SURFACE_DESC = mem::zeroed();
            (*surface).GetDesc(&mut d)?;
            d
        };
        let w = desc.Width as usize;
        let h = desc.Height as usize;
        let bytes_per_line = map.Pitch as usize;
        let total_size = bytes_per_line * h;
        let mut bgra_buf = vec![0u8; total_size];
        unsafe {
            std::ptr::copy_nonoverlapping(map.pBits, bgra_buf.as_mut_ptr(), total_size);
        }
        unsafe { (*surface).Unmap()?; }

        let frame = CapturedVideoFrame {
            bgra_data: bgra_buf,
            width: w,
            height: h,
            pts: qpc.pts_sec(),
        };

        if frame_tx.try_send(frame).is_err() {
            log_warn("DXGI frame queue full, drop frame");
        }

        unsafe {
            (*surface).Release();
            (*resource).Release();
            (*dup).ReleaseFrame();
        }
    }

    unsafe {
        (*dup).Release();
        (*output1).Release();
        (*output).Release();
        (*adapter).Release();
        (*factory).Release();
        CoUninitialize();
    }
    log_info("DXGI capture exit");
}
