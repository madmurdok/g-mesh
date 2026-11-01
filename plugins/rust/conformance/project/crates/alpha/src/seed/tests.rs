use super::*;

fn via_glob_file() -> u32 {
    schema::set_facts(4)
}

#[test]
fn glob_test() {
    assert_eq!(via_glob_file(), 4);
}
