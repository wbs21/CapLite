use crate::config::CaptureRegion;
use interactive_screenshot::prelude::*;
use tracing::{error};
pub fn pick_region(last:CaptureRegion) -> Result<CaptureRegion, String> {
    let rect = Rect::new(last.x, last.y, last.width, last.height);
    let sel = ScreenshotBuilder::new()
        .initial_rect(rect)
        .pick()
        .map_err(|e| {
            let msg = format!("选区拾取失败 {e}");
            error!("{}", msg);
            msg
        })?;
    Ok(CaptureRegion { x:sel.x, y:sel.y, width:sel.width, height:sel.height })
}
