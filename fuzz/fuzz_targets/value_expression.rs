#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(input) = std::str::from_utf8(data) else {
        return;
    };
    let Ok(parsed) = uscope::parse_value_expression(input) else {
        return;
    };

    assert!(!parsed.expression.steps.is_empty());
    assert!(matches!(
        parsed.expression.steps.first(),
        Some(uscope::ValuePathStep::Named(name)) if !name.is_empty()
    ));
    assert!(parsed.expression.steps.len() <= 64);
    if let Some(range) = parsed.range {
        assert!(input.contains(".."));
        assert!(range.start.to_string().len() <= 40);
        assert!(range.end.to_string().len() <= 40);
    }
});
