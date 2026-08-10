//! Tells cargo that the `include_str!`-ed icon files are inputs.
//!
//! Same trap as `crates/syntax/build.rs`: cargo tracks `.rs` files, and `include_str!`
//! targets are invisible to it. Without this, editing an SVG does not rebuild the crate and
//! the old glyph keeps rendering — which debugs as "the icon change didn't work".

fn main() {
    println!("cargo::rerun-if-changed=../../assets/icons");
}
