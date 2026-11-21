use crate::types::{CapturedFrame, ElementSource, Rect, UiElement};
use anyhow::{anyhow, bail, Result};
use std::cell::RefCell;
use std::collections::HashMap;
use windows::core::HSTRING;
use windows::Globalization::Language;
use windows::Graphics::Imaging::{BitmapAlphaMode, BitmapPixelFormat, SoftwareBitmap};
use windows::Media::Ocr::OcrEngine;
use windows::Storage::Streams::DataWriter;

pub fn available_languages() -> Vec<String> {
    let Ok(langs) = OcrEngine::AvailableRecognizerLanguages() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let count = langs.Size().unwrap_or(0);
    for i in 0..count {
        if let Ok(lang) = langs.GetAt(i) {
            if let Ok(tag) = lang.LanguageTag() {
                out.push(tag.to_string());
            }
        }
    }
    out
}

fn create_ocr_engine(lang_tag: Option<&str>) -> Result<OcrEngine> {
    if let Some(tag) = lang_tag.map(str::trim).filter(|s| !s.is_empty()) {
        let lang = Language::CreateLanguage(&HSTRING::from(tag))
            .map_err(|e| anyhow!("Invalid OCR language tag \"{}\": {}", tag, e))?;
        return OcrEngine::TryCreateFromLanguage(&lang).map_err(|e| {
            anyhow!(
                "OCR language \"{}\" not available (installed: {:?}): {}",
                tag,
                available_languages(),
                e
            )
        });
    }

    if let Ok(engine) = OcrEngine::TryCreateFromUserProfileLanguages() {
        return Ok(engine);
    }

    if let Ok(langs) = OcrEngine::AvailableRecognizerLanguages() {
        if langs.Size().unwrap_or(0) > 0 {
            if let Ok(first) = langs.GetAt(0) {
                if let Ok(engine) = OcrEngine::TryCreateFromLanguage(&first) {
                    return Ok(engine);
                }
            }
        }
    }

    bail!("No Windows OCR language pack available on this system.");
}

thread_local! {
    /// OCR engines are expensive to construct (language model load); reuse one per language.
    static OCR_ENGINES: RefCell<HashMap<String, OcrEngine>> = RefCell::new(HashMap::new());
}

fn cached_ocr_engine(lang_tag: Option<&str>) -> Result<OcrEngine> {
    let key = lang_tag.map(str::trim).unwrap_or_default().to_string();
    if let Some(engine) = OCR_ENGINES.with(|m| m.borrow().get(&key).cloned()) {
        return Ok(engine);
    }
    let engine = create_ocr_engine(lang_tag)?;
    OCR_ENGINES.with(|m| m.borrow_mut().insert(key, engine.clone()));
    Ok(engine)
}

/// Optional upscale factor for OCR input (`EYRA_OCR_SCALE=2|3`). Off by default: measured on
/// typical 1366x768 UI text it doubled latency without improving recognition.
fn ocr_scale(frame: &CapturedFrame, max_dim: u32) -> u32 {
    let forced = std::env::var("EYRA_OCR_SCALE")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .map(|v| v.clamp(1, 3));
    let want = forced.unwrap_or(1);
    let mut scale = want;
    while scale > 1 && (frame.width * scale > max_dim || frame.height * scale > max_dim) {
        scale -= 1;
    }
    scale
}

fn upscale_bgra(src: &[u8], w: u32, h: u32, scale: u32) -> Vec<u8> {
    if scale <= 1 {
        return src.to_vec();
    }
    let (w, h, s) = (w as usize, h as usize, scale as usize);
    let out_w = w * s;
    let mut out = vec![0u8; out_w * h * s * 4];
    for y in 0..h {
        let src_row = &src[y * w * 4..(y + 1) * w * 4];
        let dst_y0 = y * s;
        // build one upscaled row, then copy it `s` times
        let row_start = dst_y0 * out_w * 4;
        {
            let dst_row = &mut out[row_start..row_start + out_w * 4];
            for (x, px) in src_row.as_chunks::<4>().0.iter().enumerate() {
                for k in 0..s {
                    let o = (x * s + k) * 4;
                    dst_row[o..o + 4].copy_from_slice(px);
                }
            }
        }
        for k in 1..s {
            let (head, tail) = out.split_at_mut(row_start + k * out_w * 4);
            tail[..out_w * 4].copy_from_slice(&head[row_start..row_start + out_w * 4]);
        }
    }
    out
}

pub fn run_ocr(frame: &CapturedFrame, lang_tag: Option<&str>) -> Result<Vec<UiElement>> {
    if frame.width == 0 || frame.height == 0 {
        return Ok(Vec::new());
    }

    let engine = cached_ocr_engine(lang_tag)?;
    let max_dim = OcrEngine::MaxImageDimension().unwrap_or(4096);
    if frame.width > max_dim || frame.height > max_dim {
        bail!(
            "Frame {}x{} exceeds Windows OCR max dimension ({}). Capture a specific window or crop.",
            frame.width,
            frame.height,
            max_dim
        );
    }

    let scale = ocr_scale(frame, max_dim);

    // Convert RGBA -> BGRA8 for WinRT SoftwareBitmap (then optionally upscale)
    let mut bgra = frame.rgba.clone();
    for px in bgra.as_chunks_mut::<4>().0 {
        px.swap(0, 2);
    }
    let bgra = upscale_bgra(&bgra, frame.width, frame.height, scale);
    let (bmp_w, bmp_h) = (frame.width * scale, frame.height * scale);

    let writer = DataWriter::new().map_err(|e| anyhow!("DataWriter::new failed: {}", e))?;
    writer
        .WriteBytes(&bgra)
        .map_err(|e| anyhow!("DataWriter::WriteBytes failed: {}", e))?;
    let buffer = writer
        .DetachBuffer()
        .map_err(|e| anyhow!("DataWriter::DetachBuffer failed: {}", e))?;

    let bitmap = SoftwareBitmap::CreateCopyWithAlphaFromBuffer(
        &buffer,
        BitmapPixelFormat::Bgra8,
        bmp_w as i32,
        bmp_h as i32,
        BitmapAlphaMode::Premultiplied,
    )
    .map_err(|e| {
        anyhow!(
            "SoftwareBitmap::CreateCopyWithAlphaFromBuffer failed: {}",
            e
        )
    })?;

    let ocr_res = engine
        .RecognizeAsync(&bitmap)
        .map_err(|e| anyhow!("OcrEngine::RecognizeAsync failed: {}", e))?
        .get()
        .map_err(|e| anyhow!("OcrEngine recognition wait failed: {}", e))?;

    let lines = ocr_res
        .Lines()
        .map_err(|e| anyhow!("OcrResult::Lines failed: {}", e))?;

    let mut elements = Vec::new();
    let line_count = lines.Size().unwrap_or(0);
    for i in 0..line_count {
        let Ok(line) = lines.GetAt(i) else {
            continue;
        };
        let text = line
            .Text()
            .map(|h| h.to_string().trim().to_string())
            .unwrap_or_default();
        if text.is_empty() {
            continue;
        }

        // Union bounding rect across words in the line
        let mut min_x = f32::MAX;
        let mut min_y = f32::MAX;
        let mut max_x = f32::MIN;
        let mut max_y = f32::MIN;

        if let Ok(words) = line.Words() {
            let wcount = words.Size().unwrap_or(0);
            for j in 0..wcount {
                if let Ok(word) = words.GetAt(j) {
                    if let Ok(br) = word.BoundingRect() {
                        min_x = min_x.min(br.X);
                        min_y = min_y.min(br.Y);
                        max_x = max_x.max(br.X + br.Width);
                        max_y = max_y.max(br.Y + br.Height);
                    }
                }
            }
        }

        if min_x >= max_x || min_y >= max_y {
            continue;
        }

        let sf = scale as f32;
        let rect = Rect::from_ltrb(
            frame.origin_x + (min_x / sf).floor() as i32,
            frame.origin_y + (min_y / sf).floor() as i32,
            frame.origin_x + (max_x / sf).ceil() as i32,
            frame.origin_y + (max_y / sf).ceil() as i32,
        );

        elements.push(UiElement {
            id: 0,
            role: "ocr".to_string(),
            name: text,
            value: None,
            state: None,
            rect,
            source: ElementSource::Ocr,
            is_password: false,
            is_interactive: false,
            runtime_id: Vec::new(),
        });
    }

    Ok(elements)
}
