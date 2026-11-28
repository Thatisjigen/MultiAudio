//! Regenerates the app icons from `assets/icon.svg`:
//!   assets/icon.png (1024), assets/icon-256.png (window icon),
//!   assets/multiaudio.ico (Windows), assets/MultiAudio.icns (macOS).
//!
//!     cargo run --example make_icons

use std::fs;

fn render(tree: &resvg::usvg::Tree, size: u32) -> Vec<u8> {
    let mut pixmap = resvg::tiny_skia::Pixmap::new(size, size).expect("pixmap");
    let scale = size as f32 / tree.size().width();
    resvg::render(tree, resvg::tiny_skia::Transform::from_scale(scale, scale), &mut pixmap.as_mut());
    pixmap.encode_png().expect("png")
}

/// ICO with PNG-compressed entries (supported since Windows Vista).
fn ico(images: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&[0, 0, 1, 0]);
    out.extend_from_slice(&(images.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * images.len() as u32;
    for (size, png) in images {
        let b = if *size >= 256 { 0 } else { *size as u8 };
        out.extend_from_slice(&[b, b, 0, 0]);
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&32u16.to_le_bytes());
        out.extend_from_slice(&(png.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += png.len() as u32;
    }
    for (_, png) in images {
        out.extend_from_slice(png);
    }
    out
}

/// ICNS with PNG entries.
fn icns(entries: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
    let total: usize = 8 + entries.iter().map(|(_, d)| 8 + d.len()).sum::<usize>();
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(b"icns");
    out.extend_from_slice(&(total as u32).to_be_bytes());
    for (kind, data) in entries {
        out.extend_from_slice(*kind);
        out.extend_from_slice(&((8 + data.len()) as u32).to_be_bytes());
        out.extend_from_slice(data);
    }
    out
}

fn main() {
    let src = std::env::args().nth(1).unwrap_or_else(|| "assets/icon.svg".into());
    let svg = fs::read(&src).expect("read svg");
    let tree = resvg::usvg::Tree::from_data(&svg, &resvg::usvg::Options::default()).expect("parse svg");
    let png = |s| render(&tree, s);

    fs::write("assets/icon.png", png(1024)).unwrap();
    fs::write("assets/icon-256.png", png(256)).unwrap();
    let ico_sizes = [16, 24, 32, 48, 64, 128, 256];
    fs::write("assets/multiaudio.ico", ico(&ico_sizes.iter().map(|&s| (s, png(s))).collect::<Vec<_>>())).unwrap();
    let icns_entries: Vec<(&[u8; 4], Vec<u8>)> = vec![
        (b"icp4", png(16)),
        (b"icp5", png(32)),
        (b"ic11", png(32)),
        (b"icp6", png(64)),
        (b"ic12", png(64)),
        (b"ic07", png(128)),
        (b"ic08", png(256)),
        (b"ic13", png(256)),
        (b"ic09", png(512)),
        (b"ic14", png(512)),
        (b"ic10", png(1024)),
    ];
    fs::write("assets/MultiAudio.icns", icns(&icns_entries)).unwrap();
    println!("icone generate da {src}");
}
