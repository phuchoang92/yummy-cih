use super::cstr;

#[test]
fn cstr_escapes_backslash_and_single_quote() {
    assert_eq!(cstr("a\\b's"), "'a\\\\b\\'s'");
    assert_eq!(cstr("line\nnext\tcell\rend"), "'line\\nnext\\tcell\\rend'");
}
