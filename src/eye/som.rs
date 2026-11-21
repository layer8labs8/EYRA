use crate::types::{CapturedFrame, ElementSource, UiElement};

// Compact 5x7 bitmap glyphs for digits 0..9 (each row is 5 bits, MSB on left)
const DIGITS_5X7: [[u8; 7]; 10] = [
    [
        0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110,
    ], // 0
    [
        0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
    ], // 1
    [
        0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111,
    ], // 2
    [
        0b11110, 0b00001, 0b00001, 0b01110, 0b00001, 0b00001, 0b11110,
    ], // 3
    [
        0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010,
    ], // 4
    [
        0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110,
    ], // 5
    [
        0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110,
    ], // 6
    [
        0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000,
    ], // 7
    [
        0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110,
    ], // 8
    [
        0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100,
    ], // 9
];

/// Draws Set-of-Mark (SoM) bounding boxes and numbered ID badges directly onto the frame.
///
/// Two passes: all outlines first, then all badges, so a later outline never cuts through an
/// earlier badge. Each badge tries several anchor positions around its element and takes the
/// first one that does not collide with an already placed badge.
pub fn draw_som_overlay(frame: &mut CapturedFrame, elements: &[UiElement]) {
    let fw = frame.width as i32;
    let fh = frame.height as i32;
    if fw <= 0 || fh <= 0 {
        return;
    }

    let visible: Vec<(&UiElement, [i32; 4])> = elements
        .iter()
        .filter(|el| !el.is_password)
        .filter_map(|el| {
            let x0 = el.rect.x - frame.origin_x;
            let y0 = el.rect.y - frame.origin_y;
            let x1 = el.rect.right() - frame.origin_x;
            let y1 = el.rect.bottom() - frame.origin_y;
            if x1 <= 0 || y1 <= 0 || x0 >= fw || y0 >= fh {
                None
            } else {
                Some((el, [x0, y0, x1, y1]))
            }
        })
        .collect();

    for (el, [x0, y0, x1, y1]) in &visible {
        let thickness = if (x1 - x0).min(y1 - y0) < 14 { 1 } else { 2 };
        draw_rect_outline(frame, *x0, *y0, *x1, *y1, element_color(el), thickness);
    }

    let scale = 2i32;
    let pad = 2i32;
    let glyph_w = 5 * scale;
    let glyph_h = 7 * scale;
    let spacing = scale;
    let mut placed: Vec<[i32; 4]> = Vec::with_capacity(visible.len());

    for (el, [x0, y0, x1, y1]) in &visible {
        let label = el.id.to_string();
        let badge_w = (label.len() as i32) * (glyph_w + spacing) - spacing + pad * 2;
        let badge_h = glyph_h + pad * 2;
        let pos = place_badge([*x0, *y0, *x1, *y1], badge_w, badge_h, fw, fh, &placed);
        let (bx, by) = (pos[0], pos[1]);
        placed.push([bx, by, bx + badge_w, by + badge_h]);
        fill_rect(frame, bx, by, bx + badge_w, by + badge_h, element_color(el));
        draw_digits(frame, bx + pad, by + pad, &label, scale, [255, 255, 255]);
    }
}

/// Picks the first collision-free badge position from a list of anchors around the element.
fn place_badge(el: [i32; 4], bw: i32, bh: i32, fw: i32, fh: i32, placed: &[[i32; 4]]) -> [i32; 2] {
    let [x0, y0, x1, y1] = el;
    let candidates = [
        (x0, y0 - bh),      // above, left-aligned (outside: never covers the element's text)
        (x1 - bw, y0 - bh), // above, right-aligned
        (x0 - bw, y0),      // left of the element
        (x1, y0),           // right of the element
        (x0, y1),           // below, left-aligned
        (x0, y0),           // inside top-left corner
    ];
    let clamp = |(x, y): (i32, i32)| -> [i32; 2] {
        [x.clamp(0, (fw - bw).max(0)), y.clamp(0, (fh - bh).max(0))]
    };
    let collides = |p: [i32; 2]| {
        placed
            .iter()
            .any(|r| p[0] < r[2] && p[0] + bw > r[0] && p[1] < r[3] && p[1] + bh > r[1])
    };
    candidates
        .iter()
        .map(|c| clamp(*c))
        .find(|p| !collides(*p))
        .unwrap_or_else(|| clamp(candidates[0]))
}

fn element_color(el: &UiElement) -> [u8; 3] {
    if el.source == ElementSource::Ocr {
        return [155, 60, 220]; // purple for OCR
    }
    match el.role.as_str() {
        "inp" | "cmb" => [0, 140, 235],         // blue for inputs
        "btn" | "lnk" | "mnu" => [230, 55, 45], // red for buttons/links
        "chk" | "rad" | "tab" => [30, 165, 80], // green for toggles/tabs
        _ => [225, 130, 20],                    // orange for other elements
    }
}

fn set_px(frame: &mut CapturedFrame, x: i32, y: i32, rgb: [u8; 3]) {
    if x < 0 || y < 0 || x >= frame.width as i32 || y >= frame.height as i32 {
        return;
    }
    let idx = ((y as usize) * (frame.width as usize) + (x as usize)) * 4;
    frame.rgba[idx] = rgb[0];
    frame.rgba[idx + 1] = rgb[1];
    frame.rgba[idx + 2] = rgb[2];
    frame.rgba[idx + 3] = 255;
}

fn fill_rect(frame: &mut CapturedFrame, x0: i32, y0: i32, x1: i32, y1: i32, rgb: [u8; 3]) {
    let fw = frame.width as i32;
    let fh = frame.height as i32;
    let cx0 = x0.clamp(0, fw);
    let cy0 = y0.clamp(0, fh);
    let cx1 = x1.clamp(0, fw);
    let cy1 = y1.clamp(0, fh);
    for y in cy0..cy1 {
        let row = (y as usize) * (frame.width as usize) * 4;
        for x in cx0..cx1 {
            let idx = row + (x as usize) * 4;
            frame.rgba[idx] = rgb[0];
            frame.rgba[idx + 1] = rgb[1];
            frame.rgba[idx + 2] = rgb[2];
            frame.rgba[idx + 3] = 255;
        }
    }
}

fn draw_rect_outline(
    frame: &mut CapturedFrame,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    rgb: [u8; 3],
    thickness: i32,
) {
    fill_rect(frame, x0, y0, x1, y0 + thickness, rgb);
    fill_rect(frame, x0, y1 - thickness, x1, y1, rgb);
    fill_rect(frame, x0, y0, x0 + thickness, y1, rgb);
    fill_rect(frame, x1 - thickness, y0, x1, y1, rgb);
}

fn draw_digits(
    frame: &mut CapturedFrame,
    start_x: i32,
    start_y: i32,
    text: &str,
    scale: i32,
    rgb: [u8; 3],
) {
    let mut cx = start_x;
    for ch in text.chars() {
        if let Some(d) = ch.to_digit(10) {
            let glyph = DIGITS_5X7[d as usize];
            for (row_idx, bits) in glyph.iter().enumerate() {
                for col_idx in 0..5 {
                    if (bits >> (4 - col_idx)) & 1 != 0 {
                        for sy in 0..scale {
                            for sx in 0..scale {
                                set_px(
                                    frame,
                                    cx + col_idx * scale + sx,
                                    start_y + (row_idx as i32) * scale + sy,
                                    rgb,
                                );
                            }
                        }
                    }
                }
            }
            cx += 6 * scale;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Rect;

    #[test]
    fn som_overlay_draws_without_panic() {
        let mut frame = CapturedFrame {
            width: 200,
            height: 150,
            origin_x: 0,
            origin_y: 0,
            rgba: vec![30u8; 200 * 150 * 4],
        };
        let els = vec![UiElement {
            id: 12,
            role: "btn".into(),
            name: "Submit".into(),
            value: None,
            state: None,
            rect: Rect::new(40, 40, 80, 30),
            source: ElementSource::Uia,
            is_password: false,
            is_interactive: true,
            runtime_id: vec![],
        }];
        draw_som_overlay(&mut frame, &els);
        // Top border at (40, 40) should be red (230, 55, 45)
        let idx = (40 * 200 + 40) * 4;
        assert_eq!(frame.rgba[idx], 230);
        assert_eq!(frame.rgba[idx + 1], 55);
        assert_eq!(frame.rgba[idx + 2], 45);
    }

    #[test]
    fn badges_for_adjacent_elements_do_not_overlap() {
        let mut placed: Vec<[i32; 4]> = Vec::new();
        // Three tightly packed elements on the same row
        for x in [100, 112, 124] {
            let p = place_badge([x, 50, x + 10, 60], 16, 18, 400, 300, &placed);
            for r in &placed {
                let overlap = p[0] < r[2] && p[0] + 16 > r[0] && p[1] < r[3] && p[1] + 18 > r[1];
                assert!(!overlap, "badge {:?} overlaps {:?}", p, r);
            }
            placed.push([p[0], p[1], p[0] + 16, p[1] + 18]);
        }
        // Badges always stay inside the frame
        let p = place_badge([0, 0, 5, 5], 16, 18, 400, 300, &[]);
        assert!(p[0] >= 0 && p[1] >= 0);
    }
}
