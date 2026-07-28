/// Return the stable model portion of a runtime/route identifier.
///
/// ACP agents may decorate a model ID with route metadata, for example
/// `gpt-5.6-luna(openai)` or `glm-5.2[1m]`. These suffixes are useful when
/// selecting an opaque route, but they must not create a second model identity
/// in token usage.
pub fn canonical_runtime_model_id(model_id: &str) -> String {
    let mut canonical = model_id.trim();
    while let Some(base) = strip_trailing_qualifier(canonical) {
        canonical = base;
    }
    canonical.to_string()
}

/// Score two model identifiers without relying on agent/provider-specific
/// names. Higher scores represent safer matches.
///
/// Exact values always win. Lower levels progressively ignore casing, trailing
/// route annotations, provider namespaces, and display-label separators.
pub fn model_id_match_score(expected: &str, candidate: &str) -> Option<u8> {
    let expected = expected.trim();
    let candidate = candidate.trim();
    if expected.is_empty() || candidate.is_empty() {
        return None;
    }
    if expected == candidate {
        return Some(100);
    }
    if expected.eq_ignore_ascii_case(candidate) {
        return Some(95);
    }

    let expected_canonical = canonical_runtime_model_id(expected);
    let candidate_canonical = canonical_runtime_model_id(candidate);
    if expected_canonical == candidate_canonical {
        return Some(90);
    }
    if expected_canonical.eq_ignore_ascii_case(&candidate_canonical) {
        return Some(85);
    }

    let expected_bare = bare_model_id(&expected_canonical);
    let candidate_bare = bare_model_id(&candidate_canonical);
    if expected_bare.eq_ignore_ascii_case(candidate_bare) {
        return Some(80);
    }

    let expected_key = semantic_key(&expected_canonical);
    let candidate_key = semantic_key(&candidate_canonical);
    if !expected_key.is_empty() && expected_key == candidate_key {
        return Some(70);
    }

    let expected_bare_key = semantic_key(expected_bare);
    let candidate_bare_key = semantic_key(candidate_bare);
    if !expected_bare_key.is_empty() && expected_bare_key == candidate_bare_key {
        return Some(60);
    }

    None
}

pub fn model_ids_equivalent(left: &str, right: &str) -> bool {
    model_id_match_score(left, right).is_some()
}

fn strip_trailing_qualifier(value: &str) -> Option<&str> {
    let value = value.trim();
    let (open, close) = match value.chars().last()? {
        ')' => ('(', ')'),
        ']' => ('[', ']'),
        _ => return None,
    };
    debug_assert_eq!(value.chars().last(), Some(close));

    let open_index = value.rfind(open)?;
    if open_index == 0 {
        return None;
    }
    let qualifier = &value[open_index + open.len_utf8()..value.len() - close.len_utf8()];
    if qualifier.trim().is_empty()
        || qualifier.contains(open)
        || qualifier.contains(close)
        || qualifier.chars().any(char::is_control)
    {
        return None;
    }
    let base = value[..open_index].trim_end();
    (!base.is_empty()).then_some(base)
}

fn bare_model_id(model_id: &str) -> &str {
    model_id.rsplit_once('/').map_or(model_id, |(_, bare)| bare)
}

fn semantic_key(model_id: &str) -> String {
    let mut tokens = Vec::new();
    let mut token = String::new();
    for character in model_id.chars() {
        if character.is_alphanumeric() {
            token.extend(character.to_lowercase());
        } else if !token.is_empty() {
            tokens.push(std::mem::take(&mut token));
        }
    }
    if !token.is_empty() {
        tokens.push(token);
    }
    tokens.join("\0")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_repeated_route_annotations() {
        assert_eq!(
            canonical_runtime_model_id(" gpt-5.6-luna(openai)[1m] "),
            "gpt-5.6-luna"
        );
        assert_eq!(canonical_runtime_model_id("model"), "model");
        assert_eq!(canonical_runtime_model_id("(model)"), "(model)");
    }

    #[test]
    fn matches_provider_qualified_and_namespaced_model_ids() {
        assert!(model_ids_equivalent("gpt-5.6-luna", "gpt-5.6-luna(openai)"));
        assert!(model_ids_equivalent("gpt-5.6-luna", "openai/gpt-5.6-luna"));
        assert!(model_ids_equivalent(
            "GPT 5.6 Luna",
            "openai/gpt-5.6-luna(openai)"
        ));
    }

    #[test]
    fn does_not_collapse_distinct_versions() {
        assert!(!model_ids_equivalent("gpt-5.6-luna", "gpt-5.7-luna"));
        assert!(!model_ids_equivalent("gpt-5.6-luna", "gpt-56-luna"));
    }
}
