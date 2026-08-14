use super::sdp::build_service_dictionary;

#[test]
fn test_build_service_dictionary_has_required_attributes() {
    let dictionary = build_service_dictionary().unwrap();
    assert_eq!(dictionary.len(), 6);
}
