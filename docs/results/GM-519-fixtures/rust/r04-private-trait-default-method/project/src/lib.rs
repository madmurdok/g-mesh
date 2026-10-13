trait Describe {
    fn describe(&self) -> &'static str {
        "thing"
    }
}

pub struct Item;

impl Describe for Item {}

pub mod view {
    use super::{Describe, Item};

    pub fn render(i: &Item) -> &'static str {
        i.describe()
    }
}
