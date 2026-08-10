//! Tells cargo that the `include_str!`-ed query files are inputs.
//!
//! Without this, editing a `.scm` does not rebuild the crate: cargo tracks `.rs` files
//! and `include_str!` targets are invisible to it. Found the hard way — a query fix
//! appeared to do nothing until the enclosing `.rs` was touched, which is exactly the
//! kind of thing that gets debugged as "the query language is broken".

fn main() {
    println!("cargo::rerun-if-changed=queries");
}
