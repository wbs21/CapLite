use std::sync::Arc;
use std::ptr;
use std::thread;
use winapi::shared::guiddef::GUID;
use winapi::um::mmdeviceapi::*;
use winapi::um::audioclient::*;
use winapi::um::audiosessiontypes::*;
use winapi::um::combaseapi::*;
use winapi::um::synchapi::*;
use winapi::shared::winerror::*;
use uuid::Uuid;

use crate::audio_capture::TwoStageAudioRing;
use crate::logger::{log_info, log_error, log_warn};

const EVENT_TIMEOUT: u32 = 500;

pub fn start_wasapi_loopback(ring: Arc<TwoStageAudioRing>) -> std::io::Result<()> {
    unsafe {
        CoInitializeEx(ptr::null_mut(), COINIT_APARTMENTTHREADED)?;
    }

    let mut enumerator: *mut IMMDeviceEnumerator = ptr::null_mut();
    unsafe {
        CoCreateInstance(
            &CLSID_MMDeviceEnumerator,
            ptr::null_mut(),
            CLSCTX_ALL,
            &IID_IMMDeviceEnumerator,
            &mut enumerator as *mut _,
        )?;
    }
    let enumerator = enumerator;

    let mut device: *mut IMMDevice = ptr::null_mut();
    unsafe {
        (*enumerator).GetDefaultAudioEndpoint(eRender, eConsole, &mut device)?;
    }
    let device = device;

    let mut audio_client: *mut IAudioClient = ptr::null_mut();
    unsafe {
        (*device).Activate(&IID_IAudioClient, CLSCTX_ALL, ptr::null(), &mut audio_client as *mut _)?;
    }
    let audio_client = audio_client;

    let wave_fmt = winapi::shared::mmreg::WAVEFORMATEXTENSIBLE {
        Format: winapi::shared::mmreg::WAVEFORMATEX {
            wFormatTag: winapi::shared::mmreg::WAVE_FORMAT_EXTENSIBLE,
            nChannels: 2,
            nSamplesPerSec: 48000,
            nAvgBytesPerSec: 48000 * 2 * 4,
            nBlockAlign: 2 * 4,
            wBitsPerSample: 32,
            cbSize: std::mem::size_of::<winapi::shared::mmreg::WAVEFORMATEXTENSIBLE>() as u16 - std::mem::size_of::<winapi::shared::mmreg::WAVEFORMATEX>() as u16,
        },
        Samples: winapi::shared::mmreg::WAVEFORMATEXTENSIBLE_Samples { wValidBitsPerSample: 32 },
        dwChannelMask: 0b11,
        SubFormat: GUID {
            Data1: 0x00000003, Data2: 0x0000, Data3: 0x0010,
            Data4: [0x80,0x00,0x00,0xaa,0x00,0x38,0x9b,0x71],
        },
    };

    let mut buffer_period = 0u64;
    unsafe {
        (*audio_client).GetDevicePeriod(ptr::null_mut(), &mut buffer_period)?;
    }
    log_info(format!("WASAPI device period {} 100ns", buffer_period));

    unsafe {
        (*audio_client).Initialize(
            AUDIOCLIENT_SHAREMODE_SHARED,
            AUDIOCLIENT_STREAMFLAGS_LOOPBACK | AUDIOCLIENT_STREAMFLAGS_EVENTCALLBACK,
            buffer_period,
            0,
            &wave_fmt.Format,
            ptr::null(),
        )?;
    }

    let h_event = unsafe { CreateEventA(ptr::null_mut(), 0, 0, ptr::null()) };
    if h_event.is_null() {
        log_error("CreateEvent failed");
        return Err(std::io::Error::last_os_error());
    }
    unsafe { (*audio_client).SetEventHandle(h_event)?; }

    let mut render_client: *mut IAudioRenderClient = ptr::null_mut();
    unsafe {
        (*audio_client).GetService(&IID_IAudioRenderClient, &mut render_client as *mut _)?;
    }
    let render_client = render_client;

    unsafe { (*audio_client).Start()?; }
    log_info("WASAPI loopback started, event mode");

    let ring_clone = ring.clone();
    thread::spawn(move || {
        loop {
            let wait_ret = unsafe { WaitForSingleObject(h_event, EVENT_TIMEOUT) };
            match wait_ret {
                winapi::um::winbase::WAIT_OBJECT_0 => {}
                winapi::um::winbase::WAIT_TIMEOUT => continue,
                _ => {
                    log_warn("WASAPI event wait failed, exit capture thread");
                    break;
                }
            }

            let mut available_frames = 0u32;
            unsafe { (*audio_client).GetNextPacketSize(&mut available_frames)?; }
            if available_frames == 0 { continue; }

            let mut p_data: *mut u8 = ptr::null_mut();
            unsafe {
                (*render_client).GetBuffer(available_frames, &mut p_data)?;
            }
            let sample_count = available_frames as usize * 2;
            let pcm_slice = unsafe {
                std::slice::from_raw_parts(p_data as *const f32, sample_count)
            };
            ring_clone.wasapi_callback(pcm_slice);

            unsafe {
                (*render_client).ReleaseBuffer(available_frames, 0)?;
            }
        }
        unsafe {
            (*audio_client).Stop().ok();
            (*audio_client).Reset().ok();
            CloseHandle(h_event);
            (*render_client).Release();
            (*audio_client).Release();
            (*device).Release();
            (*enumerator).Release();
            CoUninitialize();
        }
    });

    Ok(())
}
