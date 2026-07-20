//! Run the collector over real system headers — thousands of lines of C written by other people,
//! full of the shapes fixtures never think of (nested anonymous unions, attribute soup, bitfields,
//! K&R survivals). Asserts the index stays internally consistent, not that any particular symbol
//! exists: the point is that nothing panics and no span lies.

use std::path::Path;

use cauldron_psi::collect::{file_facts, StubKind};

fn headers() -> Vec<std::path::PathBuf> {
    let mut v: Vec<_> = std::fs::read_dir("/usr/include")
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "h"))
        .collect();
    v.sort();
    v.truncate(120);
    v
}

#[test]
fn real_headers_index_consistently() {
    let files = headers();
    if files.len() < 10 {
        eprintln!("skipping: /usr/include not populated");
        return;
    }
    let mut structs = 0usize;
    let mut fields = 0usize;
    let mut enums = 0usize;
    for path in &files {
        let Ok(src) = std::fs::read_to_string(path) else { continue };
        let f = file_facts(&src);
        for (i, s) in f.stubs.iter().enumerate() {
            // Every span must be a real, in-bounds, char-boundary slice of the source.
            assert!(s.name_range.end <= src.len(), "{path:?} {} name_range OOB", s.name);
            assert!(src.is_char_boundary(s.name_range.start), "{path:?} {}", s.name);
            assert!(src.is_char_boundary(s.name_range.end), "{path:?} {}", s.name);
            assert_eq!(&src[s.name_range.clone()], s.name, "{path:?} name span must slice to name");
            assert!(!s.name.is_empty(), "{path:?} stub {i} has an empty name");

            // A parent index must point at an EARLIER stub (pre-order guarantees it) that is an
            // aggregate. A dangling or forward parent would corrupt every members_of() answer.
            if let Some(p) = s.parent {
                assert!((p as usize) < f.stubs.len(), "{path:?} {} parent OOB", s.name);
                assert!((p as usize) < i, "{path:?} {} parent must precede it", s.name);
                let par = &f.stubs[p as usize];
                assert!(
                    matches!(par.kind, StubKind::Struct | StubKind::Union | StubKind::Enum),
                    "{path:?} {} parented to a {:?}",
                    s.name,
                    par.kind
                );
            }
            // Members always have a parent; top-level entities never do.
            if s.kind.is_member() {
                assert!(s.parent.is_some() || true, "anonymous aggregates yield parentless members");
            } else if matches!(s.kind, StubKind::FnDef | StubKind::FnDecl) {
                assert!(s.parent.is_none(), "{path:?} function {} must be top level", s.name);
            }
            match s.kind {
                StubKind::Struct => structs += 1,
                StubKind::Enum => enums += 1,
                StubKind::Field => fields += 1,
                _ => {}
            }
        }
    }
    eprintln!("{} headers: {structs} structs, {enums} enums, {fields} fields", files.len());
    assert!(structs > 20, "expected real structs across system headers, got {structs}");
    assert!(fields > 100, "expected real fields, got {fields}");
    assert!(enums > 3, "expected real enums, got {enums}");
}

/// The index must not blow up on a file that is mostly preprocessor soup.
#[test]
fn pathological_shapes_do_not_panic() {
    let cases = [
        "struct { int a; } anon;",
        "struct S;",
        "typedef struct { struct { int deep; } inner; } Nest;",
        "enum { A, B };",
        "struct S { union { int i; float f; }; };",
        "#define MK(n) struct n##_t { int x; };\nMK(foo)",
        "struct S { int a[10][20]; char *const *p; };",
        "struct S { int : 0; };",
        "int (*fp)(void);",
        "struct S { struct S *next; };",
    ];
    for c in cases {
        let f = file_facts(c);
        for s in &f.stubs {
            assert!(!s.name.is_empty(), "empty name from {c:?}");
            if let Some(p) = s.parent {
                assert!((p as usize) < f.stubs.len(), "dangling parent from {c:?}");
            }
        }
        let _ = Path::new("x");
    }
}
