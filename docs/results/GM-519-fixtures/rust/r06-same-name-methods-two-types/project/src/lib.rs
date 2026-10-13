pub mod file {
    pub struct File;
    impl File {
        pub fn close(&self) {}
    }
}

pub mod net {
    pub struct Socket;
    impl Socket {
        pub fn close(&self) {}
    }
}

pub fn shut(f: &file::File) {
    f.close();
}
