mod limits {
    const fn base() -> usize {
        4
    }

    pub const MAX: usize = base() * 2;

    pub mod nested {
        pub const HALF: usize = super::base() / 2;
    }
}

pub fn max() -> usize {
    limits::MAX + limits::nested::HALF
}
