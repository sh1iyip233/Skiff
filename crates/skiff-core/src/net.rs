//! Helpers shared by the HTTP and API layers: error extraction and retry classification.

/// Pull Epic's `errorCode`/`errorMessage` out of a failed response body.
pub fn extract_error(text: &str) -> (String, String) {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(text) {
        let code = value
            .get("errorCode")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let message = value
            .get("errorMessage")
            .or_else(|| value.get("message"))
            .or_else(|| value.get("displayName"))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        if !code.is_empty() || !message.is_empty() {
            let fallback: String = text.chars().take(300).collect();
            return (code, if message.is_empty() { fallback } else { message });
        }
    }
    (String::new(), text.chars().take(300).collect())
}

/// Error code that means "the user has not finished the device-code login yet".
pub fn is_authorization_pending(code: &str) -> bool {
    code.ends_with("authorization_pending") || code.ends_with("slow_down")
}

/// True when an HTTP status is worth retrying.
pub fn is_retryable_status(status: u16) -> bool {
    status == 408 || status == 429 || (500..600).contains(&status)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_epic_error_payload() {
        let (code, msg) = extract_error(
            r#"{"errorCode":"errors.com.epicgames.account.oauth.authorization_pending","errorMessage":"Pending"}"#,
        );
        assert_eq!(code, "errors.com.epicgames.account.oauth.authorization_pending");
        assert_eq!(msg, "Pending");
        assert!(is_authorization_pending(&code));
    }

    #[test]
    fn falls_back_for_non_json_errors() {
        let (code, msg) = extract_error("<html>gateway timeout</html>");
        assert!(code.is_empty());
        assert!(msg.contains("gateway"));
    }

    #[test]
    fn retryable_statuses() {
        assert!(is_retryable_status(429));
        assert!(is_retryable_status(502));
        assert!(!is_retryable_status(404));
        assert!(!is_retryable_status(401));
    }
}
