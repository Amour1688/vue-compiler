use vue_compiler_core as compiler;
use compiler::compiler::{BaseCompiler, TemplateCompiler, get_base_passes};
use insta::assert_snapshot;
use rslint_parser::parse_text;

fn test_codegen(case: &str) {
    let name = insta::_macro_support::AutoName;
    let val = base_compile(case);
    // `function target have return outside function
    let wrap_in_func = format!("function () {{ {} }}", val);
    let parsed = parse_text(&wrap_in_func, 0);
    assert!(parsed.errors().is_empty());
    assert_snapshot!(name, val, case);
}

pub fn base_compile(source: &str) -> String {
    let sfc_info = Default::default();
    let option = Default::default();
    let mut ret = vec![];
    let mut compiler = BaseCompiler::new(&mut ret, get_base_passes, option);
    compiler.compile(source, &sfc_info).unwrap();
    String::from_utf8(ret).unwrap()
}

#[test]
fn test_basic_cases() {
    let cases = ["Hello world"];
    for case in cases {
        test_codegen(case);
    }
}
