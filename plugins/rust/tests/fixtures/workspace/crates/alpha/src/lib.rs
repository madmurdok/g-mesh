//! Fixture crate exercising every module-tree shape `plugins/rust`'s project
//! model has to resolve: a file-based `mod` (`nested`, itself a `mod.rs`
//! with its own further child), an inline module, and a `#[path]` module.
//! `orphan.rs`, in this same directory, is deliberately unreferenced.

mod nested;

mod inline_mod {
    pub fn helper() -> &'static str {
        "inline"
    }
}

#[path = "other_impl.rs"]
mod imp;

pub fn run() -> &'static str {
    let _ = inline_mod::helper();
    let _ = imp::detail();
    nested::deep::work()
}
