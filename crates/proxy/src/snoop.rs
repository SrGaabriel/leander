use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CursorPos {
    pub uri: String,
    pub line: u64,
    pub character: u64,
}

pub fn extract_cursor(v: &Value) -> Option<CursorPos> {
    let method = v.get("method")?.as_str()?;
    if method != "textDocument/hover" && method != "textDocument/documentHighlight" {
        return None;
    }
    let params = v.get("params")?;
    let uri = params.pointer("/textDocument/uri")?.as_str()?.to_string();
    let line = params.pointer("/position/line")?.as_u64()?;
    let character = params.pointer("/position/character")?.as_u64()?;
    Some(CursorPos {
        uri,
        line,
        character,
    })
}

pub fn is_initialized(v: &Value) -> bool {
    v.get("method").and_then(Value::as_str) == Some("initialized")
}

pub fn extract_did_open(v: &Value) -> Option<String> {
    extract_doc_uri(v, "textDocument/didOpen")
}

pub fn extract_did_close(v: &Value) -> Option<String> {
    extract_doc_uri(v, "textDocument/didClose")
}

fn extract_doc_uri(v: &Value, expected_method: &str) -> Option<String> {
    if v.get("method")?.as_str()? != expected_method {
        return None;
    }
    Some(v.pointer("/params/textDocument/uri")?.as_str()?.to_string())
}

pub fn extract_did_open_version(v: &Value) -> Option<(String, i64)> {
    extract_doc_uri_and_version(v, "textDocument/didOpen")
}

pub fn extract_did_change(v: &Value) -> Option<(String, i64)> {
    extract_doc_uri_and_version(v, "textDocument/didChange")
}

fn extract_doc_uri_and_version(v: &Value, expected_method: &str) -> Option<(String, i64)> {
    if v.get("method")?.as_str()? != expected_method {
        return None;
    }
    let uri = v.pointer("/params/textDocument/uri")?.as_str()?.to_string();
    let version = v.pointer("/params/textDocument/version")?.as_i64()?;
    Some((uri, version))
}
