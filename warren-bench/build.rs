// Copyright (c) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Builds the kit into the bench: every file under `kit/`, by its path there.

use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::{env, fs};

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .map(|e| {
            e.unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
                .path()
        })
        .collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            walk(&p, out);
        } else {
            out.push(p);
        }
    }
}

fn main() {
    println!("cargo:rerun-if-changed=kit");
    let root = Path::new(&env::var("CARGO_MANIFEST_DIR").unwrap()).join("kit");
    let mut files = Vec::new();
    walk(&root, &mut files);
    let mut code = String::from("pub const FILES: &[(&str, &[u8])] = &[\n");
    for f in &files {
        let rel: Vec<String> = f
            .strip_prefix(&root)
            .unwrap()
            .components()
            .map(|c| {
                c.as_os_str()
                    .to_str()
                    .expect("a kit path is UTF-8")
                    .to_string()
            })
            .collect();
        let abs = f.to_str().expect("a kit path is UTF-8");
        writeln!(code, "    ({:?}, include_bytes!({abs:?})),", rel.join("/")).unwrap();
    }
    code.push_str("];\n");
    let out = Path::new(&env::var("OUT_DIR").unwrap()).join("kit.rs");
    fs::write(&out, code).unwrap_or_else(|e| panic!("{}: {e}", out.display()));
}
