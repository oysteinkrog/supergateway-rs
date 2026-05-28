use supergateway_rs::cli::{check_flag_combos, FlagDiagnostic, OutputTransport};

// --stateful + non-streamableHttp output must be an error.
#[test]
fn stateful_with_sse_output_is_error() {
    let diags = check_flag_combos(OutputTransport::Sse, true, false, "");
    assert!(
        diags.iter().any(|d| matches!(d, FlagDiagnostic::Error(msg) if msg.contains("--stateful only applies"))),
        "expected error about --stateful, got: {diags:?}"
    );
}

#[test]
fn stateful_with_ws_output_is_error() {
    let diags = check_flag_combos(OutputTransport::Ws, true, false, "");
    assert!(
        diags.iter().any(|d| matches!(d, FlagDiagnostic::Error(msg) if msg.contains("--stateful only applies"))),
        "expected error about --stateful, got: {diags:?}"
    );
}

// --stateful + streamableHttp is valid — no error.
#[test]
fn stateful_with_streamable_http_is_ok() {
    let diags = check_flag_combos(OutputTransport::StreamableHttp, true, false, "");
    assert!(
        !diags.iter().any(|d| matches!(d, FlagDiagnostic::Error(_))),
        "expected no errors, got: {diags:?}"
    );
}

// --sessionTimeout without --stateful must be an error.
#[test]
fn session_timeout_without_stateful_is_error() {
    let diags = check_flag_combos(OutputTransport::Sse, false, true, "");
    assert!(
        diags.iter().any(|d| matches!(d, FlagDiagnostic::Error(msg) if msg.contains("--sessionTimeout only applies"))),
        "expected error about --sessionTimeout, got: {diags:?}"
    );
}

// --sessionTimeout with --stateful + streamableHttp is valid.
#[test]
fn session_timeout_with_stateful_is_ok() {
    let diags = check_flag_combos(OutputTransport::StreamableHttp, true, true, "");
    assert!(
        !diags.iter().any(|d| matches!(d, FlagDiagnostic::Error(_))),
        "expected no errors, got: {diags:?}"
    );
}

// --baseUrl with non-SSE output must be a warning (not an error).
#[test]
fn base_url_with_streamable_http_is_warning() {
    let diags = check_flag_combos(OutputTransport::StreamableHttp, false, false, "/prefix");
    assert!(
        diags.iter().any(|d| matches!(d, FlagDiagnostic::Warning(msg) if msg.contains("--baseUrl has no effect"))),
        "expected warning about --baseUrl, got: {diags:?}"
    );
    assert!(
        !diags.iter().any(|d| matches!(d, FlagDiagnostic::Error(_))),
        "expected no errors, got: {diags:?}"
    );
}

#[test]
fn base_url_with_ws_is_warning() {
    let diags = check_flag_combos(OutputTransport::Ws, false, false, "/prefix");
    assert!(
        diags.iter().any(|d| matches!(d, FlagDiagnostic::Warning(_))),
        "expected warning about --baseUrl, got: {diags:?}"
    );
    assert!(
        !diags.iter().any(|d| matches!(d, FlagDiagnostic::Error(_))),
        "expected no errors, got: {diags:?}"
    );
}

// --baseUrl with SSE output is fine — no diagnostics.
#[test]
fn base_url_with_sse_is_ok() {
    let diags = check_flag_combos(OutputTransport::Sse, false, false, "/prefix");
    assert!(diags.is_empty(), "expected no diagnostics, got: {diags:?}");
}

// Empty --baseUrl with any mode produces no diagnostics.
#[test]
fn empty_base_url_no_diagnostics() {
    let diags = check_flag_combos(OutputTransport::StreamableHttp, false, false, "");
    assert!(diags.is_empty(), "expected no diagnostics, got: {diags:?}");
}
