use crate::types::{CapturedFrame, Rect};
use anyhow::{anyhow, bail, Result};
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::imageops::FilterType;
use image::{ColorType, ImageEncoder, RgbaImage};
use std::ffi::c_void;
use std::mem::size_of;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetDIBits,
    ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, CAPTUREBLT, DIB_RGB_COLORS,
    SRCCOPY,
};
use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};
use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;

const PW_RENDERFULLCONTENT: u32 = 0x0000_0002;

pub fn capture_screen(rect: Rect) -> Result<CapturedFrame> {
    if !rect.is_valid() {
        bail!("Invalid capture rectangle: {:?}", rect);
    }

    let w = rect.w;
    let h = rect.h;

    unsafe {
        let screen_dc = GetDC(HWND::default());
        if screen_dc.is_invalid() {
            bail!("GetDC(NULL) failed.");
        }

        let mem_dc = CreateCompatibleDC(screen_dc);
        if mem_dc.is_invalid() {
            ReleaseDC(HWND::default(), screen_dc);
            bail!("CreateCompatibleDC failed.");
        }

        let bmp = CreateCompatibleBitmap(screen_dc, w, h);
        if bmp.is_invalid() {
            let _ = DeleteDC(mem_dc);
            ReleaseDC(HWND::default(), screen_dc);
            bail!("CreateCompatibleBitmap failed for {}x{}.", w, h);
        }

        let old = SelectObject(mem_dc, bmp);
        let blt_res = BitBlt(
            mem_dc,
            0,
            0,
            w,
            h,
            screen_dc,
            rect.x,
            rect.y,
            SRCCOPY | CAPTUREBLT,
        );

        let rgba = if blt_res.is_ok() {
            read_dib_rgba(mem_dc, bmp, w, h)
        } else {
            let err = windows::core::Error::from_win32();
            Err(anyhow!(
                "BitBlt failed (desktop may be locked or non-interactive): {}",
                err
            ))
        };

        SelectObject(mem_dc, old);
        let _ = DeleteObject(bmp);
        let _ = DeleteDC(mem_dc);
        ReleaseDC(HWND::default(), screen_dc);

        Ok(CapturedFrame {
            width: w as u32,
            height: h as u32,
            origin_x: rect.x,
            origin_y: rect.y,
            rgba: rgba?,
        })
    }
}

/// Captures a specific window using PrintWindow(PW_RENDERFULLCONTENT) — works even when
/// the window is occluded by other windows (similar to OBS Window Capture).
/// Automatically falls back to screen BitBlt if PrintWindow returns a blank frame.
pub fn capture_window(hwnd_val: usize, visual_rect: Rect) -> Result<CapturedFrame> {
    let hwnd = HWND(hwnd_val as *mut c_void);

    let mut raw_wr = RECT::default();
    let win_rect = unsafe {
        if GetWindowRect(hwnd, &mut raw_wr).is_ok()
            && raw_wr.right > raw_wr.left
            && raw_wr.bottom > raw_wr.top
        {
            Rect::from_ltrb(raw_wr.left, raw_wr.top, raw_wr.right, raw_wr.bottom)
        } else {
            visual_rect
        }
    };

    if !win_rect.is_valid() {
        bail!("Target window has zero or invalid dimensions (may be minimized).");
    }

    let w = win_rect.w;
    let h = win_rect.h;

    let printed = unsafe {
        let screen_dc = GetDC(HWND::default());
        if screen_dc.is_invalid() {
            None
        } else {
            let mem_dc = CreateCompatibleDC(screen_dc);
            let bmp = CreateCompatibleBitmap(screen_dc, w, h);
            let res = if !mem_dc.is_invalid() && !bmp.is_invalid() {
                let old = SelectObject(mem_dc, bmp);
                let ok =
                    PrintWindow(hwnd, mem_dc, PRINT_WINDOW_FLAGS(PW_RENDERFULLCONTENT)).as_bool();
                let data = if ok {
                    read_dib_rgba(mem_dc, bmp, w, h).ok()
                } else {
                    None
                };
                SelectObject(mem_dc, old);
                data
            } else {
                None
            };
            if !bmp.is_invalid() {
                let _ = DeleteObject(bmp);
            }
            if !mem_dc.is_invalid() {
                let _ = DeleteDC(mem_dc);
            }
            ReleaseDC(HWND::default(), screen_dc);
            res
        }
    };

    if let Some(rgba) = printed {
        if has_non_blank_pixels(&rgba) {
            let full_frame = CapturedFrame {
                width: w as u32,
                height: h as u32,
                origin_x: win_rect.x,
                origin_y: win_rect.y,
                rgba,
            };
            // Align tightly to DWM visual bounds if inset from GetWindowRect shadow frame
            if visual_rect.is_valid() && visual_rect != win_rect {
                if let Some(clipped) = visual_rect.clamp_to(&win_rect) {
                    if let Ok(cropped) = crop_frame(&full_frame, clipped) {
                        return Ok(cropped);
                    }
                }
            }
            return Ok(full_frame);
        }
    }

    // Fallback: capture from desktop surface at visual_rect
    capture_screen(if visual_rect.is_valid() {
        visual_rect
    } else {
        win_rect
    })
}

unsafe fn read_dib_rgba(
    hdc: windows::Win32::Graphics::Gdi::HDC,
    hbmp: windows::Win32::Graphics::Gdi::HBITMAP,
    w: i32,
    h: i32,
) -> Result<Vec<u8>> {
    let mut bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h, // top-down DIB
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };

    let len = (w as usize) * (h as usize) * 4;
    let mut buf = vec![0u8; len];
    let lines = GetDIBits(
        hdc,
        hbmp,
        0,
        h as u32,
        Some(buf.as_mut_ptr() as *mut c_void),
        &mut bmi,
        DIB_RGB_COLORS,
    );
    if lines == 0 {
        bail!("GetDIBits failed.");
    }

    // Convert BGRA -> RGBA with opaque alpha
    for px in buf.as_chunks_mut::<4>().0 {
        px.swap(0, 2);
        px[3] = 255;
    }

    Ok(buf)
}

fn has_non_blank_pixels(rgba: &[u8]) -> bool {
    // Sample across the image to quickly detect an all-black PrintWindow failure
    let step = (rgba.len() / 4 / 512).max(1) * 4;
    let mut i = 0;
    while i + 2 < rgba.len() {
        if rgba[i] != 0 || rgba[i + 1] != 0 || rgba[i + 2] != 0 {
            return true;
        }
        i += step;
    }
    false
}

/// Masks sensitive rectangles (in screen coordinates) with solid dark boxes.
pub fn apply_redactions(frame: &mut CapturedFrame, rects: &[Rect]) {
    let fw = frame.width as i32;
    let fh = frame.height as i32;
    for r in rects {
        let x0 = (r.x - frame.origin_x).clamp(0, fw);
        let y0 = (r.y - frame.origin_y).clamp(0, fh);
        let x1 = (r.right() - frame.origin_x).clamp(0, fw);
        let y1 = (r.bottom() - frame.origin_y).clamp(0, fh);
        if x1 <= x0 || y1 <= y0 {
            continue;
        }
        for y in y0..y1 {
            let row = (y as usize) * (frame.width as usize) * 4;
            for x in x0..x1 {
                let idx = row + (x as usize) * 4;
                frame.rgba[idx] = 18;
                frame.rgba[idx + 1] = 18;
                frame.rgba[idx + 2] = 24;
                frame.rgba[idx + 3] = 255;
            }
        }
    }
}

/// Crops a CapturedFrame using screen-space coordinates.
pub fn crop_frame(frame: &CapturedFrame, screen_rect: Rect) -> Result<CapturedFrame> {
    let frame_rect = frame.rect();
    let Some(clipped) = screen_rect.clamp_to(&frame_rect) else {
        bail!(
            "Crop rectangle {:?} does not intersect captured frame {:?}",
            screen_rect,
            frame_rect
        );
    };

    let rel_x = (clipped.x - frame.origin_x) as usize;
    let rel_y = (clipped.y - frame.origin_y) as usize;
    let cw = clipped.w as usize;
    let ch = clipped.h as usize;
    let src_stride = (frame.width as usize) * 4;
    let dst_stride = cw * 4;

    let mut out = vec![0u8; ch * dst_stride];
    for row in 0..ch {
        let src_off = (rel_y + row) * src_stride + rel_x * 4;
        let dst_off = row * dst_stride;
        out[dst_off..dst_off + dst_stride]
            .copy_from_slice(&frame.rgba[src_off..src_off + dst_stride]);
    }

    Ok(CapturedFrame {
        width: cw as u32,
        height: ch as u32,
        origin_x: clipped.x,
        origin_y: clipped.y,
        rgba: out,
    })
}

/// Encodes a frame into compact JPEG (default) or PNG bytes, resizing proportionally if wider than max_width.
pub fn encode_frame(
    frame: &CapturedFrame,
    max_width: u32,
    quality: u8,
    format: &str,
) -> Result<(Vec<u8>, u32, u32, &'static str)> {
    let img = RgbaImage::from_raw(frame.width, frame.height, frame.rgba.clone())
        .ok_or_else(|| anyhow!("Failed to construct RGBA image buffer."))?;

    let resized = if max_width > 0 && img.width() > max_width {
        let scale = max_width as f64 / img.width() as f64;
        let new_h = ((img.height() as f64 * scale).round() as u32).max(1);
        image::imageops::resize(&img, max_width, new_h, FilterType::Triangle)
    } else {
        img
    };

    let out_w = resized.width();
    let out_h = resized.height();
    let mut bytes = Vec::new();

    if format.eq_ignore_ascii_case("png") {
        let enc = PngEncoder::new(&mut bytes);
        enc.write_image(resized.as_raw(), out_w, out_h, ColorType::Rgba8.into())?;
        Ok((bytes, out_w, out_h, "image/png"))
    } else {
        let rgb = image::DynamicImage::ImageRgba8(resized).to_rgb8();
        let q = quality.clamp(20, 95);
        let mut enc = JpegEncoder::new_with_quality(&mut bytes, q);
        enc.encode(rgb.as_raw(), out_w, out_h, ColorType::Rgb8.into())?;
        Ok((bytes, out_w, out_h, "image/jpeg"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redaction_blacks_out_exact_region() {
        let mut frame = CapturedFrame {
            width: 20,
            height: 20,
            origin_x: 100,
            origin_y: 200,
            rgba: vec![255u8; 20 * 20 * 4],
        };
        apply_redactions(&mut frame, &[Rect::new(105, 205, 5, 5)]);
        // Pixel inside redacted region (rel 6,6)
        let inside = (6 * 20 + 6) * 4;
        assert_eq!(frame.rgba[inside], 18);
        // Pixel outside (rel 0,0)
        assert_eq!(frame.rgba[0], 255);
    }

    #[test]
    fn crop_and_encode_jpeg_works() {
        let frame = CapturedFrame {
            width: 64,
            height: 48,
            origin_x: 10,
            origin_y: 20,
            rgba: vec![120u8; 64 * 48 * 4],
        };
        let cropped = crop_frame(&frame, Rect::new(20, 30, 32, 24)).unwrap();
        assert_eq!(cropped.width, 32);
        assert_eq!(cropped.height, 24);
        assert_eq!(cropped.origin_x, 20);
        assert_eq!(cropped.origin_y, 30);

        let (bytes, w, h, mime) = encode_frame(&cropped, 16, 70, "jpeg").unwrap();
        assert_eq!(mime, "image/jpeg");
        assert_eq!(w, 16);
        assert_eq!(h, 12);
        assert!(!bytes.is_empty());
    }
}
