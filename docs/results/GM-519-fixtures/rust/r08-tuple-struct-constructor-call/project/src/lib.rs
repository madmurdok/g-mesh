pub struct Pair(pub u8);

pub enum Shape {
    Dot(u8),
}

pub fn make() -> Pair {
    Pair(1)
}

pub fn dot() -> Shape {
    Shape::Dot(1)
}
