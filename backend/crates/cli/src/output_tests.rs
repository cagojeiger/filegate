use super::*;
use serde_json::json;

#[test]
fn byte_display_keeps_zero_negative_and_large_values() {
    assert_eq!(bytes(0), "0 B");
    assert_eq!(bytes(-1), "-1 B");
    assert_eq!(bytes(-1024), "-1.0 KiB");
    assert_eq!(bytes(1024 * 1024), "1.0 MiB");
    assert_eq!(bytes(i64::MIN), "-8.0 EiB");
}

#[test]
fn table_escapes_terminal_controls_and_keeps_empty_inventory_explicit() {
    let rendered = table(&json!(["a\u{1b}[31m\nb"]));
    assert!(!rendered.contains('\u{1b}'));
    assert_eq!(rendered.lines().count(), 2);
    assert!(rendered.contains("\\n"));
    assert_eq!(table(&json!([])), "(empty)\n");
}

#[test]
fn nested_status_fields_and_signed_usage_are_visible() {
    let rendered =
        table(&json!({"registry":{"state":"unknown","client_count":null},"remaining_bytes":-1024}));
    assert!(rendered.contains("registry.state"));
    assert!(rendered.contains("-1.0 KiB"));
    assert!(rendered.contains("unknown"));
}
