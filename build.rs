use pulldown_cmark::{Parser, html};
use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo::rerun-if-changed=README.md");
    println!("cargo::rerun-if-changed=static/index.html");

    let mut readme_html = String::new();
    html::push_html(&mut readme_html, Parser::new(include_str!("README.md")));
    let page = include_str!("static/index.html").replace("<!-- README -->", &readme_html);
    let output = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo output directory"));
    fs::write(output.join("index.html"), page).expect("write documentation page");
}
