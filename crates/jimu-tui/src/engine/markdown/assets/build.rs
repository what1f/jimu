#[cfg(feature = "syntax")]
fn main() {
    use std::env;
    use std::path::Path;

    use syntect::dumps::dump_to_uncompressed_file;
    use syntect::parsing::SyntaxDefinition;

    println!("cargo:rerun-if-changed=src/engine/markdown/assets/Swift.sublime-syntax");
    // Runtime `SyntaxSet::build` of two-face stalls first Read paint (GB-5513 PTY fold).
    let swift = SyntaxDefinition::load_from_str(include_str!("Swift.sublime-syntax"), true, None)
        .expect("parse Swift.sublime-syntax");
    let mut builder = two_face::syntax::extra_newlines().into_builder();
    builder.add(swift);
    let set = builder.build();
    dump_to_uncompressed_file(
        &set,
        Path::new(&env::var("OUT_DIR").unwrap()).join("syntaxes.bin"),
    )
    .expect("dump syntaxes");
}

#[cfg(not(feature = "syntax"))]
fn main() {}
