pub struct Meter {
    value: u32,
}

impl Meter {
    fn read(&self) -> u32 {
        self.value
    }
}

mod report {
    use super::Meter;

    pub fn show(m: &Meter) -> u32 {
        m.read()
    }
}

pub fn total(m: &Meter) -> u32 {
    report::show(m) + m.read()
}
