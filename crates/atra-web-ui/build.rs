use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=tailwind.css");
    let output =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("assets/tailwind.css");
    if !output.exists() {
        fs::create_dir_all(output.parent().unwrap()).unwrap();
        fs::File::create(output).unwrap();
    }
}
