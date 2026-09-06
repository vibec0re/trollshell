//! **Scratch harness for issue #930** ("the gauge is a bit too blurry").
//!
//! Renders the default [`Gauge`] in three states × two skins and writes each as
//! a PNG (plus a raw RGBA sidecar so a later run can tile a contact sheet).
//!
//! The *candidate* is whatever the working tree's `gauge.rs` currently is — the
//! harness never overrides it. One commit per candidate, one run per commit:
//!
//! ```sh
//! cargo run -p hytte-preem --example gauge_candidates -- <outdir> emit A
//! # …edit gauge.rs, commit, rebuild…
//! cargo run -p hytte-preem --example gauge_candidates -- <outdir> emit B
//! # …then, once every candidate is on disk:
//! cargo run -p hytte-preem --example gauge_candidates -- <outdir> sheet A B C D E F
//! ```
//!
//! Not part of the library, not shipped, no new dependency: the PNG encoder
//! below is ~70 lines of stored-deflate zlib.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::cast_possible_wrap
)]

use std::io::Write as _;

use hytte_preem::{DisplayStyle, Frame, Gauge, Rgba};

/// One frame at 60 Hz — the cadence the states are stepped at.
const FRAME: f32 = 1.0 / 60.0;
/// Where the needle rests before the step.
const REST: f32 = 0.30;
/// Where it is sent.
const STEP: f32 = 0.80;
/// Frames after the step for the "mid-swing" state. 6 frames = 0.10 s, which is
/// within a millisecond of the default spring's peak velocity
/// (t = atan(√(1-ζ²)/ζ)/ω_d = 0.0963 s for ω = 2π·2, ζ = 0.5).
const SWING_FRAMES: usize = 6;
/// Frames for the "settled" state — 2 s, far past `is_settled()`.
const SETTLE_FRAMES: usize = 120;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(dir) = args.first() else {
        eprintln!("usage: gauge_candidates <outdir> emit <name> | <outdir> sheet <name>...");
        std::process::exit(2);
    };
    match args.get(1).map(String::as_str) {
        Some("emit") => {
            let name = args.get(2).cloned().unwrap_or_else(|| "A".to_owned());
            emit(dir, &name);
        }
        Some("sheet") => sheet(dir, &args[2..]),
        _ => {
            eprintln!("usage: gauge_candidates <outdir> emit <name> | <outdir> sheet <name>...");
            std::process::exit(2);
        }
    }
}

// ── States ───────────────────────────────────────────────────────────────────

/// The three states, in sheet-column order.
const STATES: [&str; 3] = ["rest30", "swing", "settled80"];

/// A gauge parked at rest at [`REST`].
fn at_rest() -> Gauge {
    let mut gauge = Gauge::new();
    gauge.set_target(REST);
    gauge.settle();
    gauge
}

/// The gauge in `state`, plus a one-line description of where the needle is.
fn staged(state: &str) -> (Gauge, String) {
    let mut gauge = at_rest();
    match state {
        "rest30" => {}
        "swing" => {
            gauge.set_target(STEP);
            for _ in 0..SWING_FRAMES {
                gauge.advance(FRAME);
            }
        }
        "settled80" => {
            gauge.set_target(STEP);
            for _ in 0..SETTLE_FRAMES {
                gauge.advance(FRAME);
            }
        }
        other => panic!("unknown state {other}"),
    }
    let note = format!(
        "fraction={:.4} velocity={:.4}/s settled={}",
        gauge.fraction(),
        gauge.needle().velocity(),
        gauge.is_settled()
    );
    (gauge, note)
}

/// The two skins the issue cares about, in sheet order.
fn skins() -> [(&'static str, DisplayStyle); 2] {
    [("vfd", DisplayStyle::Vfd), ("lcd", DisplayStyle::Lcd)]
}

// ── Emit ─────────────────────────────────────────────────────────────────────

fn emit(dir: &str, candidate: &str) {
    for (skin_name, style) in skins() {
        for state in STATES {
            let (gauge, note) = staged(state);
            let frame = gauge.render(style);
            let stem = format!("gauge-{skin_name}-{candidate}-{state}");
            write_png(&format!("{dir}/{stem}.png"), &frame);
            write_raw(&format!("{dir}/{stem}.rgba"), &frame);
            println!(
                "{stem}.png  {}x{}  digest={:08x}  {note}",
                frame.width(),
                frame.height(),
                digest(frame.data())
            );
        }
    }
}

/// A cheap FNV-1a over the buffer — enough to tell two candidates apart (and to
/// prove a candidate that should be a no-op for a skin really is one).
fn digest(bytes: &[u8]) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for &b in bytes {
        h ^= u32::from(b);
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

// ── Contact sheet ────────────────────────────────────────────────────────────

/// Gutter/header thickness in px, and the separator width.
const GUTTER: usize = 14;
const SEP: usize = 1;
/// Sheet chrome colours.
const SHEET_BG: Rgba = [0x18, 0x18, 0x1c, 0xff];
const SHEET_SEP: Rgba = [0x50, 0x50, 0x58, 0xff];
const SHEET_MARK: Rgba = [0xff, 0xff, 0xff, 0xff];

fn sheet(dir: &str, candidates: &[String]) {
    for (skin_name, _) in skins() {
        let mut tiles: Vec<Vec<Frame>> = Vec::new();
        for candidate in candidates {
            let mut row = Vec::new();
            for state in STATES {
                let path = format!("{dir}/gauge-{skin_name}-{candidate}-{state}.rgba");
                row.push(read_raw(&path));
            }
            tiles.push(row);
        }
        let (tw, th) = (tiles[0][0].width(), tiles[0][0].height());
        let cols = STATES.len();
        let rows = candidates.len();
        let width = GUTTER + cols * (tw + SEP);
        let height = GUTTER + rows * (th + SEP);
        let mut sheet = Frame::filled(width, height, SHEET_BG);

        for (r, row) in tiles.iter().enumerate() {
            let y = GUTTER + r * (th + SEP);
            // Row marker: r+1 pips down the left gutter.
            for i in 0..=r {
                sheet.rect(2, (y + 3 + i * 5) as i32, 4, 4, SHEET_MARK);
            }
            for (c, tile) in row.iter().enumerate() {
                let x = GUTTER + c * (tw + SEP);
                sheet.blit(tile, x as i32, y as i32);
                if r == 0 {
                    // Column marker: c+1 pips along the top header.
                    for i in 0..=c {
                        sheet.rect((x + 3 + i * 5) as i32, 3, 4, 4, SHEET_MARK);
                    }
                }
                // Separators to the right of and below each tile.
                sheet.rect((x + tw) as i32, y as i32, SEP as i32, th as i32, SHEET_SEP);
                sheet.rect(x as i32, (y + th) as i32, tw as i32, SEP as i32, SHEET_SEP);
            }
        }
        let path = format!("{dir}/sheet-{skin_name}.png");
        write_png(&path, &sheet);
        println!(
            "sheet-{skin_name}.png  {width}x{height}  rows(top→bottom)={} cols(left→right)={}",
            candidates.join(","),
            STATES.join(",")
        );
    }
}

// ── Raw sidecar ──────────────────────────────────────────────────────────────

fn write_raw(path: &str, frame: &Frame) {
    let mut out = Vec::with_capacity(8 + frame.data().len());
    out.extend_from_slice(&(frame.width() as u32).to_le_bytes());
    out.extend_from_slice(&(frame.height() as u32).to_le_bytes());
    out.extend_from_slice(frame.data());
    std::fs::write(path, out).expect("write raw");
}

fn read_raw(path: &str) -> Frame {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    let w = u32::from_le_bytes(bytes[0..4].try_into().unwrap()) as usize;
    let h = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
    let mut frame = Frame::new(w, h);
    let px = &bytes[8..];
    for y in 0..h {
        for x in 0..w {
            let o = (y * w + x) * 4;
            frame.plot(x as i32, y as i32, [px[o], px[o + 1], px[o + 2], px[o + 3]]);
        }
    }
    frame
}

// ── Minimal PNG encoder (stored-deflate zlib; no dependency) ─────────────────

fn write_png(path: &str, frame: &Frame) {
    let (w, h) = (frame.width(), frame.height());
    // Filter byte 0 in front of every scanline.
    let mut raw = Vec::with_capacity(h * (1 + w * 4));
    for y in 0..h {
        raw.push(0u8);
        let row = &frame.data()[y * w * 4..(y + 1) * w * 4];
        raw.extend_from_slice(row);
    }

    let mut png: Vec<u8> = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&(w as u32).to_be_bytes());
    ihdr.extend_from_slice(&(h as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit, RGBA, deflate, no filter/interlace
    chunk(&mut png, b"IHDR", &ihdr);
    chunk(&mut png, b"IDAT", &zlib_stored(&raw));
    chunk(&mut png, b"IEND", &[]);

    let mut f = std::fs::File::create(path).unwrap_or_else(|e| panic!("create {path}: {e}"));
    f.write_all(&png).expect("write png");
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], body: &[u8]) {
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(body);
    let mut crc_input = Vec::with_capacity(4 + body.len());
    crc_input.extend_from_slice(kind);
    crc_input.extend_from_slice(body);
    out.extend_from_slice(&crc32(&crc_input).to_be_bytes());
}

/// A zlib stream of *stored* (uncompressed) deflate blocks. PNG only requires
/// the data to be a valid zlib stream; nothing says it has to be small.
fn zlib_stored(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    let mut rest = data;
    loop {
        let take = rest.len().min(0xffff);
        let (block, tail) = rest.split_at(take);
        let final_block = u8::from(tail.is_empty());
        out.push(final_block);
        out.extend_from_slice(&(take as u16).to_le_bytes());
        out.extend_from_slice(&(!(take as u16)).to_le_bytes());
        out.extend_from_slice(block);
        if tail.is_empty() {
            break;
        }
        rest = tail;
    }
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            let mask = 0u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in data {
        a = (a + u32::from(byte)) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}
