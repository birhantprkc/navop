use gpui_component::highlighter::{LanguageRegistry, SyntaxHighlighter};

#[test]
fn markdown_editor_does_not_bundle_a_native_rust_grammar() {
    let registry = LanguageRegistry::singleton();
    assert!(
        registry.language("rust").is_none(),
        "fenced languages must be supplied by language extensions, not native Cargo features"
    );
    assert_eq!(
        "text",
        SyntaxHighlighter::new("rust").language().as_ref(),
        "an unavailable fenced language must safely fall back to text"
    );
}
