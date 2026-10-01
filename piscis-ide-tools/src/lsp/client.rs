//! Direct stdio JSON-RPC client for language servers.
//!
//! Used by the agent-facing `lsp` and `read_lints` tools. Unlike the Monaco
//! WebSocket bridge (one connection, canned `initialize`), this client performs
//! the real LSP handshake, keeps the server alive across tool calls, answers
//! server-initiated requests, and caches `publishDiagnostics` per document.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{oneshot, Mutex, Notify};
use tracing::{debug, warn};

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

struct DocState {
    version: i32,
    text: String,
}

struct DiagEntry {
    generation: u64,
    items: Vec<Value>,
}

pub struct LspClient {
    root: String,
    language: String,
    stdin: Mutex<ChildStdin>,
    pending: Pending,
    next_id: AtomicU64,
    alive: Arc<AtomicBool>,
    docs: Mutex<HashMap<String, DocState>>,
    diags: Arc<Mutex<HashMap<String, DiagEntry>>>,
    diag_gen: Arc<AtomicU64>,
    diag_notify: Arc<Notify>,
    stderr_tail: Arc<std::sync::Mutex<Vec<String>>>,
    _child: Mutex<Child>,
}

impl LspClient {
    /// Spawn `command args` in `root`, run the initialize handshake.
    pub async fn spawn(
        command: &str,
        args: &[String],
        root: &str,
        language: &str,
    ) -> Result<Arc<Self>, String> {
        let mut child = piscis_kernel::proc::tokio_command(command)
            .args(args)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("failed to spawn '{command}': {e}"))?;
        let stdin = child.stdin.take().ok_or("no stdin handle")?;
        let stdout = child.stdout.take().ok_or("no stdout handle")?;
        let stderr = child.stderr.take();

        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let alive = Arc::new(AtomicBool::new(true));
        let diags = Arc::new(Mutex::new(HashMap::new()));
        let diag_gen = Arc::new(AtomicU64::new(0));
        let diag_notify = Arc::new(Notify::new());
        let stderr_tail: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();

        let client = Arc::new(Self {
            root: root.to_string(),
            language: language.to_string(),
            stdin: Mutex::new(stdin),
            pending: pending.clone(),
            next_id: AtomicU64::new(1),
            alive: alive.clone(),
            docs: Mutex::new(HashMap::new()),
            diags: diags.clone(),
            diag_gen: diag_gen.clone(),
            diag_notify: diag_notify.clone(),
            stderr_tail: stderr_tail.clone(),
            _child: Mutex::new(child),
        });

        if let Some(stderr) = stderr {
            let tail = stderr_tail.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    debug!("lsp stderr: {line}");
                    if let Ok(mut t) = tail.lock() {
                        t.push(line);
                        if t.len() > 8 {
                            t.remove(0);
                        }
                    }
                }
            });
        }

        let weak = Arc::downgrade(&client);
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            loop {
                match read_message(&mut reader).await {
                    Ok(Some(msg)) => {
                        let Some(c) = weak.upgrade() else { break };
                        c.dispatch(msg).await;
                    }
                    Ok(None) => break,
                    Err(e) => {
                        warn!("lsp read error: {e}");
                        break;
                    }
                }
            }
            alive.store(false, Ordering::SeqCst);
            let mut p = pending.lock().await;
            for (_, tx) in p.drain() {
                let _ = tx.send(Err("language server exited".into()));
            }
        });

        let root_uri = path_to_uri(root);
        let init = json!({
            "processId": std::process::id(),
            "rootUri": root_uri,
            "workspaceFolders": [{ "uri": root_uri, "name": "project" }],
            "capabilities": {
                "workspace": { "workspaceFolders": true, "configuration": true },
                "textDocument": {
                    "synchronization": { "didSave": false },
                    "publishDiagnostics": { "relatedInformation": true },
                    "hover": { "contentFormat": ["markdown", "plaintext"] },
                    "completion": { "completionItem": { "snippetSupport": false } },
                    "definition": { "linkSupport": true },
                    "references": {},
                    "rename": { "prepareSupport": false },
                    "documentSymbol": { "hierarchicalDocumentSymbolSupport": true }
                }
            }
        });
        client
            .request("initialize", init, Duration::from_secs(60))
            .await
            .map_err(|e| {
                // Give the stderr reader a moment to capture the server's own explanation.
                let tail = client
                    .stderr_tail
                    .lock()
                    .map(|t| t.join(" | "))
                    .unwrap_or_default();
                if tail.is_empty() {
                    format!("LSP initialize failed: {e}")
                } else {
                    format!("LSP initialize failed: {e} (server said: {tail})")
                }
            })?;
        client.notify("initialized", json!({})).await?;
        Ok(client)
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    pub fn root(&self) -> &str {
        &self.root
    }

    pub fn language(&self) -> &str {
        &self.language
    }

    async fn send(&self, msg: &Value) -> Result<(), String> {
        if !self.is_alive() {
            return Err("language server is not running".into());
        }
        let body = serde_json::to_string(msg).map_err(|e| e.to_string())?;
        let mut stdin = self.stdin.lock().await;
        let frame = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
        stdin
            .write_all(frame.as_bytes())
            .await
            .map_err(|e| format!("write to language server failed: {e}"))?;
        stdin.flush().await.map_err(|e| e.to_string())
    }

    pub async fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        self.send(&json!({ "jsonrpc": "2.0", "method": method, "params": params }))
            .await
    }

    pub async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        if let Err(e) = self
            .send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .await
        {
            self.pending.lock().await.remove(&id);
            return Err(e);
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(res)) => res,
            Ok(Err(_)) => Err("language server closed the request".into()),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(format!("{method} timed out after {}s", timeout.as_secs()))
            }
        }
    }

    async fn dispatch(&self, msg: Value) {
        let method = msg.get("method").and_then(|m| m.as_str());
        let id = msg.get("id").cloned();
        match (method, id) {
            (Some(method), Some(id)) => {
                let result = match method {
                    "workspace/configuration" => {
                        let n = msg
                            .pointer("/params/items")
                            .and_then(|i| i.as_array())
                            .map_or(0, |a| a.len());
                        Value::Array(vec![Value::Null; n])
                    }
                    _ => Value::Null,
                };
                let _ = self
                    .send(&json!({ "jsonrpc": "2.0", "id": id, "result": result }))
                    .await;
            }
            (Some("textDocument/publishDiagnostics"), None) => {
                if let Some(params) = msg.get("params") {
                    let uri = params.get("uri").and_then(|u| u.as_str()).unwrap_or("");
                    let items = params
                        .get("diagnostics")
                        .and_then(|d| d.as_array())
                        .cloned()
                        .unwrap_or_default();
                    let generation = self.diag_gen.fetch_add(1, Ordering::SeqCst) + 1;
                    self.diags
                        .lock()
                        .await
                        .insert(uri.to_string(), DiagEntry { generation, items });
                    self.diag_notify.notify_waiters();
                }
            }
            (None, Some(id)) => {
                let Some(id) = id.as_u64() else { return };
                let outcome = if let Some(err) = msg.get("error") {
                    Err(err
                        .get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("unknown LSP error")
                        .to_string())
                } else {
                    Ok(msg.get("result").cloned().unwrap_or(Value::Null))
                };
                if let Some(tx) = self.pending.lock().await.remove(&id) {
                    let _ = tx.send(outcome);
                }
            }
            _ => {}
        }
    }

    /// Open the file (or push a full-text change if it was edited since), and
    /// return its URI. Returns whether the server saw new content.
    pub async fn sync_file(&self, path: &str, language_id: &str) -> Result<(String, bool), String> {
        let raw = std::fs::read(path).map_err(|e| format!("cannot read {path}: {e}"))?;
        if raw.len() > 4 * 1024 * 1024 {
            return Err("file too large (>4MB)".into());
        }
        let text = String::from_utf8_lossy(&raw).to_string();
        let uri = path_to_uri(path);
        let mut docs = self.docs.lock().await;
        match docs.get_mut(&uri) {
            None => {
                docs.insert(
                    uri.clone(),
                    DocState {
                        version: 1,
                        text: text.clone(),
                    },
                );
                drop(docs);
                self.notify(
                    "textDocument/didOpen",
                    json!({ "textDocument": {
                        "uri": uri, "languageId": language_id, "version": 1, "text": text
                    }}),
                )
                .await?;
                Ok((uri, true))
            }
            Some(doc) if doc.text != text => {
                doc.version += 1;
                doc.text = text.clone();
                let version = doc.version;
                drop(docs);
                self.notify(
                    "textDocument/didChange",
                    json!({
                        "textDocument": { "uri": uri, "version": version },
                        "contentChanges": [{ "text": text }]
                    }),
                )
                .await?;
                Ok((uri, true))
            }
            Some(_) => Ok((uri, false)),
        }
    }

    /// Diagnostics for a synced document: waits for a fresh publish when the
    /// content changed, then falls back to a pull request.
    pub async fn diagnostics(
        &self,
        uri: &str,
        content_changed: bool,
        wait: Duration,
    ) -> Vec<Value> {
        let baseline = if content_changed {
            self.diag_gen.load(Ordering::SeqCst)
        } else {
            0
        };
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let notified = self.diag_notify.notified();
            {
                let map = self.diags.lock().await;
                if let Some(entry) = map.get(uri) {
                    if entry.generation > baseline {
                        return entry.items.clone();
                    }
                }
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() || !self.is_alive() {
                break;
            }
            let _ = tokio::time::timeout(remaining, notified).await;
        }
        if let Ok(res) = self
            .request(
                "textDocument/diagnostic",
                json!({ "textDocument": { "uri": uri } }),
                Duration::from_secs(4),
            )
            .await
        {
            if let Some(items) = res.get("items").and_then(|i| i.as_array()) {
                return items.clone();
            }
        }
        let map = self.diags.lock().await;
        map.get(uri).map(|e| e.items.clone()).unwrap_or_default()
    }
}

async fn read_message<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
) -> Result<Option<Value>, String> {
    let mut content_len: Option<usize> = None;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await.map_err(|e| e.to_string())?;
        if n == 0 {
            return Ok(None);
        }
        let line = line.trim_end();
        if line.is_empty() {
            if content_len.is_some() {
                break;
            }
            continue;
        }
        if let Some(v) = line
            .to_ascii_lowercase()
            .strip_prefix("content-length:")
            .map(str::trim)
        {
            content_len = v.parse().ok();
        }
    }
    let len = content_len.ok_or("missing Content-Length")?;
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).await.map_err(|e| e.to_string())?;
    serde_json::from_slice(&buf)
        .map(Some)
        .map_err(|e| format!("invalid JSON from server: {e}"))
}

/// `C:\a b\x.rs` -> `file:///C:/a%20b/x.rs`; `/a/x.rs` -> `file:///a/x.rs`.
pub fn path_to_uri(path: &str) -> String {
    let norm = path.replace('\\', "/");
    let mut out = String::from("file://");
    if !norm.starts_with('/') {
        out.push('/');
    }
    for b in norm.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' | b':' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Inverse of [`path_to_uri`] (display only).
pub fn uri_to_path(uri: &str) -> String {
    let rest = uri.strip_prefix("file://").unwrap_or(uri);
    let bytes = rest.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if let Some(v) = rest
                .get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
            {
                decoded.push(v);
                i += 3;
                continue;
            }
        }
        decoded.push(bytes[i]);
        i += 1;
    }
    let s = String::from_utf8_lossy(&decoded).to_string();
    let b = s.as_bytes();
    if b.len() > 2 && b[0] == b'/' && b[2] == b':' && b[1].is_ascii_alphabetic() {
        s[1..].to_string()
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_round_trip_windows_and_unix() {
        let u = path_to_uri(r"C:\My Proj\src\a.rs");
        assert_eq!(u, "file:///C:/My%20Proj/src/a.rs");
        assert_eq!(uri_to_path(&u), "C:/My Proj/src/a.rs");
        assert_eq!(path_to_uri("/a/b.rs"), "file:///a/b.rs");
        assert_eq!(uri_to_path("file:///a/b.rs"), "/a/b.rs");
    }

    #[tokio::test]
    async fn reads_framed_message() {
        let body = r#"{"jsonrpc":"2.0","id":1,"result":null}"#;
        let frame = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
        let mut r = BufReader::new(frame.as_bytes());
        let msg = read_message(&mut r).await.unwrap().unwrap();
        assert_eq!(msg["id"], 1);
        assert!(read_message(&mut r).await.unwrap().is_none());
    }
}
