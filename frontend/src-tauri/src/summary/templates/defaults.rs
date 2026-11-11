/// Embedded default templates using compile-time inclusion
///
/// These templates are bundled into the binary and serve as fallbacks
/// when custom templates are not available.

/// Standard meeting notes template, the default when no template is chosen
pub const STANDARD_MEETING: &str = include_str!("../../../templates/standard_meeting.json");

/// Registry of all built-in templates
///
/// Maps template identifiers to their embedded JSON content
pub fn get_builtin_templates() -> Vec<(&'static str, &'static str)> {
    vec![("standard_meeting", STANDARD_MEETING)]
}

/// Get a built-in template by identifier
///
/// # Arguments
/// * `id` - Template identifier (e.g., "standard_meeting")
///
/// # Returns
/// The template JSON content if found, None otherwise
pub fn get_builtin_template(id: &str) -> Option<&'static str> {
    match id {
        "standard_meeting" => Some(STANDARD_MEETING),
        _ => None,
    }
}

/// List all built-in template identifiers
pub fn list_builtin_template_ids() -> Vec<&'static str> {
    vec!["standard_meeting"]
}

/// Template IDs that shipped in earlier versions, mapped to the template that
/// replaced them. Saved settings (the pipeline's `summary_template_id`) and
/// summary requests may still name a retired ID; without this they would fail
/// with "Template not found".
pub fn replacement_for_retired_template(id: &str) -> Option<&'static str> {
    match id {
        "daily_standup" | "project_sync" => Some("delivery_sync"),
        "sales_marketing_client_call" => Some("client_account"),
        "retrospective" | "psychatric_session" => Some("standard_meeting"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builtin_templates_valid_json() {
        for (id, content) in get_builtin_templates() {
            let result = serde_json::from_str::<serde_json::Value>(content);
            assert!(
                result.is_ok(),
                "Built-in template '{}' contains invalid JSON: {:?}",
                id,
                result.err()
            );
        }
    }

    #[test]
    fn test_get_builtin_template() {
        assert!(get_builtin_template("standard_meeting").is_some());
        assert!(get_builtin_template("nonexistent").is_none());
    }
}
