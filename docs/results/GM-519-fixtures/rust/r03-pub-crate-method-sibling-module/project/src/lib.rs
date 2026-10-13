pub mod store {
    pub struct Store;

    impl Store {
        pub(crate) fn flush(&self) -> bool {
            true
        }
    }
}

pub mod api {
    use crate::store::Store;

    pub fn save(s: &Store) -> bool {
        s.flush()
    }
}
