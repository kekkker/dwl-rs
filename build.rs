// Like dwl's config.h: create src/config.rs from the defaults on first build.
fn main() {
    println!("cargo:rerun-if-changed=src/config.rs");
    if !std::path::Path::new("src/config.rs").exists() {
        std::fs::copy("src/config.def.rs", "src/config.rs").expect("copy src/config.def.rs to src/config.rs");
    }
}
