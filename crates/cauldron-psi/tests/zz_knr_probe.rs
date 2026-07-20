use cauldron_psi::extract::plan;

fn run(src: &str, needle: &str, label: &str) {
    let mut p = tree_sitter::Parser::new();
    p.set_language(&tree_sitter_c::language()).unwrap();
    let t = p.parse(src, None).unwrap();
    println!("=== {label} ===");
    println!("sexp: {}", t.root_node().to_sexp());
    println!("has_error: {}", t.root_node().has_error());
    let start = src.find(needle).unwrap();
    let sel = start..start + needle.len();
    match plan(src, sel, "extracted") {
        Ok(pl) => {
            println!("PLAN OK\nfunction_text:\n{}call: {}\nparams: {:?}", pl.function_text, pl.call_text, pl.params);
        }
        Err(e) => println!("REFUSED: {e:?} / {e}"),
    }
}

#[test]
fn probe() {
    let knr = "int run(a, b)\nint a;\nint b;\n{\n    use(a);\n    return b;\n}\n";
    run(knr, "    use(a);", "K&R");
    let fp = "int (*run(int n))(int)\n{\n    use(n);\n    return 0;\n}\n";
    run(fp, "    use(n);", "fnptr-return");
}
