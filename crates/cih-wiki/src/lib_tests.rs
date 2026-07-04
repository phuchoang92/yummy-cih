use super::clean_method_desc;

#[test]
fn clean_method_desc_strips_fqcn_prefix_with_arity() {
    let result = clean_method_desc(
        "DelinquencyApiResource.updateDelinquencyBucket/2() processes the bucket update.",
        "DelinquencyApiResource",
        "updateDelinquencyBucket",
    );
    assert_eq!(result, "Processes the bucket update.");
}

#[test]
fn clean_method_desc_strips_backtick_quoted_classname() {
    let result = clean_method_desc(
        "`DelinquencyApiResource`.updateDelinquencyBucket/2() processes the bucket update.",
        "DelinquencyApiResource",
        "updateDelinquencyBucket",
    );
    assert_eq!(result, "Processes the bucket update.");
}

#[test]
fn clean_method_desc_strips_connective_phrase_after_paren() {
    let result = clean_method_desc(
        "The resource method ClassName.foo/0() is called to validate the input.",
        "ClassName",
        "foo",
    );
    assert_eq!(result, "Validate the input.");
}

#[test]
fn clean_method_desc_leaves_clean_input_unchanged() {
    let result = clean_method_desc(
        "Validates the payment amount before processing.",
        "SomeClass",
        "someMethod",
    );
    assert_eq!(result, "Validates the payment amount before processing.");
}
