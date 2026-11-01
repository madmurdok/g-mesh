//! Two fns of one module, each declaring a local `Nothing` with its own
//! impl of `Extractor`: two impl blocks, qualified by their fns.

pub trait Extractor {
    fn run(&self) -> u32;
}

pub fn first() -> u32 {
    struct Nothing;
    impl Extractor for Nothing {
        fn run(&self) -> u32 {
            1
        }
    }
    Nothing.run()
}

pub fn second() -> u32 {
    struct Nothing;
    impl Extractor for Nothing {
        fn run(&self) -> u32 {
            2
        }
    }
    Nothing.run()
}
