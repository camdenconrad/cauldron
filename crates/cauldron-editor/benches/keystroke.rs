//! Gate-A bench: per-keystroke CPU cost on a large C file, headless.
//!
//! Measures the two costs the editor pays per keystroke:
//!   1. buffer apply + incremental tree-sitter reparse,
//!   2. egui text layout for a viewport of ~50 visible lines (one LayoutJob per line).
//! Explicitly NOT measured: wgpu upload/present/vblank (see docs/phase0.md — the honest budget is
//! ≤8 ms p99 for these CPU legs; end-to-end ≤16 ms is Phase-3 acceptance in the real app).
//!
//! Point CAULDRON_BENCH_FILE at a real cFS .c (e.g. cfe/modules/es/fsw/src/cfe_es_api.c);
//! falls back to a synthesized ~5k-line C file so the bench always runs.

use criterion::{criterion_group, criterion_main, Criterion};

use cauldron_editor::{buffer::Transaction, syntax::{Lang, Syntax}, Buffer};

fn load_source() -> String {
    if let Ok(p) = std::env::var("CAULDRON_BENCH_FILE") {
        if let Ok(s) = std::fs::read_to_string(&p) {
            eprintln!("bench file: {p} ({} lines)", s.lines().count());
            return s;
        }
    }
    // Synthesize ~5k lines of plausible C.
    let mut s = String::from("#include <stdint.h>\n\n");
    for i in 0..500 {
        s.push_str(&format!(
            "int32_t CFE_Fake_Func{i}(uint32_t a, uint32_t b) {{\n    uint32_t acc = a;\n    for (uint32_t k = 0; k < b; ++k) {{\n        acc += k ^ a;\n        if (acc > 1000000u) {{ acc %= 7u; }}\n    }}\n    /* comment line for density */\n    return (int32_t)acc;\n}}\n\n"
        ));
    }
    s
}

fn bench_keystroke(c: &mut Criterion) {
    let src = load_source();

    c.bench_function("apply+reparse (insert 1 char mid-file)", |bch| {
        let mut buf = Buffer::from_text(&src);
        let mut syn = Syntax::new(Lang::C, buf.rope()).unwrap();
        let at = src.len() / 2;
        // find a char boundary
        let mut at = at;
        while !src.is_char_boundary(at) {
            at -= 1;
        }
        bch.iter(|| {
            let tx = Transaction::insert(at, "x");
            buf.apply(&tx);
            syn.edited(buf.rope(), &tx.changes);
            // undo so the buffer doesn't grow unboundedly across iterations
            buf.undo();
            let del = Transaction::delete(at, at + 1);
            syn.edited(buf.rope(), &del.changes);
        });
    });

    c.bench_function("viewport layout (50 lines, egui LayoutJob+galley)", |bch| {
        let buf = Buffer::from_text(&src);
        let fonts = egui::epaint::text::Fonts::new(
            1.0,
            2048,
            egui::FontDefinitions::default(),
        );
        let font_id = egui::FontId::monospace(14.0);
        let total_lines = buf.rope().len_lines();
        let first = (total_lines / 2).saturating_sub(25);
        bch.iter(|| {
            fonts.begin_pass(1.0, 2048);
            let mut px = 0.0f32;
            for l in first..(first + 50).min(total_lines) {
                let line: String = buf.rope().line(l).to_string();
                let galley = fonts.layout_job(egui::text::LayoutJob::simple(
                    line,
                    font_id.clone(),
                    egui::Color32::WHITE,
                    f32::INFINITY,
                ));
                px += galley.size().x;
            }
            let _ = fonts.font_image_delta();
            criterion::black_box(px)
        });
    });
}

criterion_group!(benches, bench_keystroke);
criterion_main!(benches);
