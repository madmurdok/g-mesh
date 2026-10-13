pub struct Gauge;

mod imp {
    impl super::Gauge {
        pub(super) fn level(&self) -> u8 {
            7
        }
    }
}

pub fn probe(g: &Gauge) -> u8 {
    g.level()
}
