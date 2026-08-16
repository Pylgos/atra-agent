use std::{
    env, fs,
    path::{Path, PathBuf},
};

fn main() {
    println!("cargo:rerun-if-env-changed=ATRA_WEB_ASSETS_DIR");
    println!("cargo:rerun-if-changed=assets");
    let root = env::var_os("ATRA_WEB_ASSETS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("assets"));
    println!("cargo:rerun-if-changed={}", root.display());
    let mut files = Vec::new();
    collect(&root, &root, &mut files);
    files.sort();
    let mut generated = String::from(
        "pub fn get(path: &str) -> Option<(&'static [u8], &'static str)> {\nmatch path {\n",
    );
    for (relative, absolute) in files {
        let route = format!("/{}", relative.to_string_lossy().replace('\\', "/"));
        let mime = mime(&route);
        generated.push_str(&format!(
            "{route:?} => Some((include_bytes!({absolute:?}), {mime:?})),\n",
            absolute = absolute.canonicalize().unwrap().to_string_lossy()
        ));
    }
    generated.push_str("_ => None,\n}\n}\n");
    fs::write(
        PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("assets.rs"),
        generated,
    )
    .unwrap();
}

fn collect(root: &Path, directory: &Path, files: &mut Vec<(PathBuf, PathBuf)>) {
    for entry in fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            collect(root, &path, files);
        } else {
            files.push((path.strip_prefix(root).unwrap().to_owned(), path));
        }
    }
}

fn mime(path: &str) -> &'static str {
    if path.ends_with(".html") {
        "text/html; charset=utf-8"
    } else if path.ends_with(".css") {
        "text/css; charset=utf-8"
    } else if path.ends_with(".js") {
        "text/javascript; charset=utf-8"
    } else if path.ends_with(".wasm") {
        "application/wasm"
    } else if path.ends_with(".svg") {
        "image/svg+xml"
    } else {
        "application/octet-stream"
    }
}
