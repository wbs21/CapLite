//! 音频设备枚举与选择。

use crate::error::AppError;
use wasapi::{initialize_mta, DeviceCollection, DeviceEnumerator, Direction};
use windows::core::HRESULT;

/// 线程已初始化 COM 但单元模型不同：`RPC_E_CHANGED_MODE`（0x80010106）。
///
/// 这不是"失败"：UI 主线程被 winit/eframe 初始化成 **STA**（窗口、拖放都要 STA），
/// 我们再调 `CoInitializeEx(MTA)` 必然拿到这个值。WASAPI 在 STA 下工作完全正常
/// （MTA 只是惯例偏好），所以它必须和 `S_OK` / `S_FALSE` 一样被当作"COM 已就绪"。
/// 之前把这里当错误，直接导致"仅声音模式"启动录制失败：
/// `音频采集失败：没有可用的音源：System 音源打开失败：… CoInitializeEx(MTA) 失败`。
const RPC_E_CHANGED_MODE: i32 = 0x8001_0106u32 as i32;

/// 把**本线程**初始化成 STA（单线程单元）。
///
/// 存在的唯一理由：`src/bin/audio_probe.rs --sta` 要真实复现"eframe/winit 已经把主线程
/// 初始化成 STA"这一现场，然后验证 [`ensure_com()`] 能在这种线程上继续工作。
/// 真机程序自己不需要调它——UI 主线程的 STA 是 winit 建的。
pub fn initialize_sta_for_probe() -> HRESULT {
    wasapi::initialize_sta()
}

/// `CoInitializeEx` 的返回值是否表示"COM 现在可用"。
///
/// * `S_OK`(0)：本次真的初始化了；
/// * `S_FALSE`(1)：本线程早已初始化过，引用计数 +1；
/// * `RPC_E_CHANGED_MODE`：已按别的单元模型初始化过（eframe 主线程就是这种）。
///
/// 其余值（`E_INVALIDARG`、`E_OUTOFMEMORY`、`RPC_E_TOO_LATE` 等）才是真错误。
/// 公开出来是为了让 `src/bin/audio_probe.rs` 能刻意"先 STA 再打开"来复现/验证本 bug。
pub fn com_ready(hr: HRESULT) -> bool {
    hr.is_ok() || hr.0 == RPC_E_CHANGED_MODE
}

/// WASAPI 需要先初始化 COM。本模块所有公开函数都会先确保这一点，
/// 这样调用方（UI 线程、采集线程、测试）都不必关心——**包括主线程已经是 STA 的情况**。
pub fn ensure_com() -> Result<(), AppError> {
    let hr = initialize_mta();
    if com_ready(hr) {
        if hr.0 == RPC_E_CHANGED_MODE {
            tracing::debug!("本线程 COM 已是 STA（winit/eframe 主线程），直接在 STA 下使用 WASAPI");
        }
        Ok(())
    } else {
        Err(AppError::Audio(format!("CoInitializeEx(MTA) 失败: {hr:?}")))
    }
}

/// 一个音频端点
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioDevice {
    /// 设备 id（WASAPI endpoint id），持久化到配置里
    pub id: String,
    /// 友好名称，用于界面显示
    pub name: String,
    /// 是否系统默认设备
    pub is_default: bool,
}

impl AudioDevice {
    pub fn label(&self) -> String {
        if self.is_default {
            format!("{}（默认）", self.name)
        } else {
            self.name.clone()
        }
    }
}

fn collect(collection: DeviceCollection, default_id: Option<String>) -> Result<Vec<AudioDevice>, AppError> {
    let count = collection.get_nbr_devices().unwrap_or(0);
    let mut out = Vec::with_capacity(count as usize);
    for i in 0..count {
        if let Ok(device) = collection.get_device_at_index(i) {
            let name = device
                .get_friendlyname()
                .unwrap_or_else(|_| format!("音频设备 {i}"));
            let id = device.get_id().unwrap_or_default();
            out.push(AudioDevice {
                is_default: default_id.as_deref() == Some(id.as_str()),
                id,
                name,
            });
        }
    }
    Ok(out)
}

fn default_id(direction: &Direction) -> Option<String> {
    DeviceEnumerator::new()
        .ok()?
        .get_default_device(direction)
        .ok()?
        .get_id()
        .ok()
}

/// 枚举所有**渲染设备**（用于系统声音环回采集）
pub fn list_render_devices() -> Result<Vec<AudioDevice>, AppError> {
    ensure_com()?;
    let enumr = DeviceEnumerator::new()
        .map_err(|e| AppError::Audio(format!("创建设备枚举器失败: {e}")))?;
    let col = enumr
        .get_device_collection(&Direction::Render)
        .map_err(|e| AppError::Audio(format!("枚举渲染设备失败: {e}")))?;
    collect(col, default_id(&Direction::Render))
}

/// 枚举所有**采集设备**（麦克风）
pub fn list_capture_devices() -> Result<Vec<AudioDevice>, AppError> {
    ensure_com()?;
    let enumr = DeviceEnumerator::new()
        .map_err(|e| AppError::Audio(format!("创建设备枚举器失败: {e}")))?;
    let col = enumr
        .get_device_collection(&Direction::Capture)
        .map_err(|e| AppError::Audio(format!("枚举采集设备失败: {e}")))?;
    collect(col, default_id(&Direction::Capture))
}

/// 麦克风设备：优先按 id 匹配，找不到或未指定则用系统默认设备
pub fn resolve_mic(device_id: Option<&str>) -> Result<wasapi::Device, AppError> {
    ensure_com()?;
    let enumr = DeviceEnumerator::new()
        .map_err(|e| AppError::Audio(format!("创建设备枚举器失败: {e}")))?;
    if let Some(id) = device_id {
        if let Ok(d) = enumr.get_device(id) {
            return Ok(d);
        }
        tracing::warn!(id, "配置的麦克风不存在，回落默认设备");
    }
    enumr
        .get_default_device(&Direction::Capture)
        .map_err(|e| AppError::Audio(format!("取默认麦克风失败: {e}")))
}

/// 系统声音：默认渲染设备（环回采集）
pub fn resolve_render(device_id: Option<&str>) -> Result<wasapi::Device, AppError> {
    ensure_com()?;
    let enumr = DeviceEnumerator::new()
        .map_err(|e| AppError::Audio(format!("创建设备枚举器失败: {e}")))?;
    if let Some(id) = device_id {
        if let Ok(d) = enumr.get_device(id) {
            return Ok(d);
        }
        tracing::warn!(id, "配置的扬声器不存在，回落默认设备");
    }
    enumr
        .get_default_device(&Direction::Render)
        .map_err(|e| AppError::Audio(format!("取默认渲染设备失败: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::capture::{AudioCapture, SourceKind};

    #[test]
    fn enumerates_audio_endpoints_on_this_machine() {
        let render = list_render_devices();
        let capture = list_capture_devices();
        match (&render, &capture) {
            (Ok(r), Ok(c)) => {
                println!(
                    "渲染设备 {} 个: {:?}",
                    r.len(),
                    r.iter().map(|d| d.label()).collect::<Vec<_>>()
                );
                println!(
                    "采集设备 {} 个: {:?}",
                    c.len(),
                    c.iter().map(|d| d.label()).collect::<Vec<_>>()
                );
            }
            _ => println!("枚举失败: render={render:?} capture={capture:?}"),
        }
        // 本机至少应有默认渲染设备（否则系统声音无法录制，要给出明确错误而不是崩）
        assert!(render.is_ok(), "渲染设备枚举不应报错");
    }

    /// 回归：UI 主线程被 winit/eframe 初始化成 **STA**，本模块再调 `CoInitializeEx(MTA)`
    /// 会返回 `RPC_E_CHANGED_MODE`（0x80010106）。复现步骤就是"先 STA，再走正常打开流程"。
    #[test]
    fn open_audio_capture_succeeds_on_sta_thread() {
        // 初次 STA 初始化：返回 S_OK 或 S_FALSE（本线程已初始化）都算成功
        let hr = wasapi::initialize_sta();
        assert!(com_ready(hr), "本线程 STA 初始化失败：{hr:?}");

        let device = resolve_render(None).expect("STA 下也应能取到默认渲染设备");
        let cap = AudioCapture::open(SourceKind::System, device)
            .expect("STA 线程上打开系统声音采集必须成功（RPC_E_CHANGED_MODE 不是错误）");
        println!(
            "STA 下打开系统声音成功，缓冲区 {} 帧",
            cap.buffer_frames()
        );
    }

    #[test]
    fn default_devices_resolve_without_id() {
        assert!(resolve_render(None).is_ok(), "应能取到默认渲染设备");
        // 麦克风可能不存在（笔记本无麦/被禁用），所以只要求不 panic
        match resolve_mic(None) {
            Ok(d) => println!("默认麦克风: {:?}", d.get_friendlyname()),
            Err(e) => println!("无可用默认麦克风: {e}"),
        }
    }
}
