use super::util::{invalid_symbol_selector_spec_error, split_field_path};
use anyhow::Result;
use kclvm_ast::ast;

/// Parse symbol selector string to symbol selector spec
///
/// # Examples
///
/// ```
/// use kclvm_query::selector::parse_symbol_selector_spec;
///
/// if let Ok(spec) = parse_symbol_selector_spec("", "alice.age") {
///     assert_eq!(spec.pkgpath, "".to_string());
///     assert_eq!(spec.field_path, "alice.age".to_string());
/// }
/// ```
pub fn parse_symbol_selector_spec(
    pkg_root: &str,
    symbol_path: &str,
    source_codes: Vec<String>,
) -> Result<ast::SymbolSelectorSpec> {
    if let Ok((pkgpath, field_path)) = split_field_path(symbol_path) {
        Ok(ast::SymbolSelectorSpec {
            pkg_root: pkg_root.to_string(),
            pkgpath,
            field_path,
            source_codes: if source_codes.is_empty() {
                None
            } else {
                Some(source_codes)
            },
        })
    } else {
        Err(invalid_symbol_selector_spec_error(symbol_path))
    }
}

#[test]
fn test_symbol_path_selector() {
    let spec = parse_symbol_selector_spec("", "pkg_name:alice.age", vec![]).unwrap();
    assert_eq!(spec.pkgpath, "pkg_name".to_string());
    assert_eq!(spec.field_path, "alice.age".to_string());
}
