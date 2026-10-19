use windows::{
    core::*,
    Graphics::Capture::*,
    Graphics::DirectX::Direct3D11::*,
    Graphics::DirectX::*,
    Win32::Graphics::Direct3D11::*,
    Win32::Graphics::Dxgi::*,
};
use crate::config::CaptureRegion;
use tracing::{error};

pub struct WgcCapture {
    session: GraphicsCaptureSession,
    pool: Direct3D11CaptureFramePool,
    region: CaptureRegion,
}

impl WgcCapture {
    pub fn new(region: CaptureRegion) -> Result<Self, String> {
        let item = GraphicsCaptureItem::GetForPrimaryDisplay().map_err(|e| e.to_string())?;
        let d3d_device: ID3D11Device = unsafe {
            let mut dev = None;
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                None,
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                &mut dev,
                None,
                None
            ).map_err(|e| format!("D3D create device fail: {e}"))?;
            dev.unwrap()
        };
        let dxgi_dev: IDXGIDevice = d3d_device.cast().map_err(|e| e.to_string())?;
        let direct_dev = CreateDirect3DDevice(&dxgi_dev).map_err(|e| e.to_string())?;

        let item_size = item.Size();
        let pool = Direct3D11CaptureFramePool::Create(
            &direct_dev,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            2,
            item_size
        ).map_err(|e| e.to_string())?;

        let session = pool.CreateCaptureSession(&item).map_err(|e| e.to_string())?;
        session.IsCursorCaptureEnabled(true).map_err(|e| e.to_string())?;
        session.StartCapture().map_err(|e| e.to_string())?;

        Ok(Self {
            session,
            pool,
            region,
        })
    }

    pub fn next_frame(&self) -> Result<Option<Vec<u8>>, String> {
        let frame = match self.pool.TryGetNextFrame() {
            Ok(f) => f,
            Err(_) => return Ok(None),
        };
        let surface = frame.Surface().map_err(|e| e.to_string())?;
        let tex2d: ID3D11Texture2D = surface.cast().map_err(|e| e.to_string())?;

        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { tex2d.GetDesc(&mut desc); }

        let crop_x = self.region.x as usize;
        let crop_y = self.region.y as usize;
        let crop_w = self.region.width as usize;
        let crop_h = self.region.height as usize;

        let tex_width = desc.Width as usize;
        let tex_height = desc.Height as usize;
        if crop_x + crop_w > tex_width || crop_y + crop_h > tex_height {
            return Err("选区超出采集画面范围".to_string());
        }

        let mut staging_desc = desc;
        staging_desc.Usage = D3D11_USAGE_STAGING;
        staging_desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        staging_desc.BindFlags = 0;
        staging_desc.MiscFlags = 0;

        let mut staging_tex: Option<ID3D11Texture2D> = None;
        let d3d_ctx = unsafe {
            let mut ctx = None;
            let mut tmp_ctx: *mut std::ffi::c_void = std::ptr::null_mut();
            tex2d.GetDevice(None).unwrap().GetImmediateContext(&mut tmp_ctx).unwrap();
            ID3D11DeviceContext::from_raw(tmp_ctx as _)
        };
        unsafe {
            D3D11CreateTexture2D(
                Some(&d3d_ctx),
                &staging_desc,
                None,
                &mut staging_tex
            ).map_err(|e| format!("创建staging纹理失败: {e}"))?;
        }
        let staging_tex = staging_tex.unwrap();

        unsafe {
            d3d_ctx.CopyResource(&staging_tex, &tex2d);
        }

        let mut mapped: D3D11_MAPPED_SUBRESOURCE = Default::default();
        unsafe {
            d3d_ctx.Map(
                &staging_tex,
                0,
                D3D11_MAP_READ,
                0,
                &mut mapped
            ).map_err(|e| format!("Map staging texture failed: {e}"))?;
        }
        let row_pitch = mapped.RowPitch as usize;
        let src_ptr = mapped.pData as *const u8;

        let mut out_buf = vec![0u8; crop_w * crop_h * 4];

        unsafe {
            for y in 0..crop_h {
                let src_line_start = src_ptr.add((crop_y + y) * row_pitch);
                let src_pixel_start = src_line_start.add(crop_x * 4);
                let dst_slice = &mut out_buf[y * crop_w * 4..(y + 1) * crop_w * 4];
                std::ptr::copy_nonoverlapping(src_pixel_start, dst_slice.as_mut_ptr(), crop_w * 4);
            }
            d3d_ctx.Unmap(&staging_tex, 0);
        }

        Ok(Some(out_buf))
    }
}
