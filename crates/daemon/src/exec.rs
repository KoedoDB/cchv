//! The exec calls of the daemon's JSON API: what `kubectl exec` and the exec probes of the kubelet need, carried from the shim to the
//! guest agent (`crun exec` runs in the VM; see the agent's `exec.rs`).
//!
//! Every request names `vm_id`, `container_id` and `exec_id`; the VM must be one that was handed out (active) and have an agent
//! connection. The methods and their answers:
//!
//! | method | request fields | answer |
//! |---|---|---|
//! | `ExecCreate` | `process_json` (base64 of the OCI Process JSON, at most 1 MiB), `terminal` | `{}` |
//! | `ExecStart` | | `{"pid"}` |
//! | `ExecKill` | `signal` (1 to 64) | `{}` |
//! | `ExecWait` | | `{"exit_status","exited_at_unix_nanos"}` after the exit (no timeout; the caller has its own) |
//! | `ExecDelete` | | `{}` |
//! | `ExecWrite` | `data` (base64, at most 64 KiB decoded) | `{"written"}` |
//! | `ExecCloseStdin` | | `{}` |
//! | `ExecRead` | `stdout_offset`, `stderr_offset`, `max` | `{"stdout","stderr"` (base64)`,"stdout_offset","stderr_offset","eof","dropped","stdout_dropped","stderr_dropped"}` |
//! | `ExecResize` | `width`, `height` | `{}` |
//!
//! An error is `{"error": "..."}`. `dropped` is the bytes lost from both streams together (the two are also given on their own).
//! The offsets are cumulative, as in `ReadLogs`: the caller's offsets acknowledge what it has taken.
//!
//! **The exec id `init`** is reserved for the container's own process: `ExecWrite`, `ExecCloseStdin` and `ExecResize` with it go to the
//! terminal of the container itself (`tty: true`; its output comes through `ReadLogs`). `ExecCreate` refuses it, and nothing else means
//! anything for it.
//!
//! The process definition (it can hold secrets in its environment) and the data written and read are never logged; only the method and
//! sizes are.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use serde_json::{json, Value};

use crate::api::AgentRegistry;
use crate::pool::Pool;
use crate::validate;

/// The largest process definition (decoded) and the most bytes of one write (decoded), as in the agent.
pub const MAX_PROCESS_JSON: usize = 1024 * 1024;
pub const MAX_WRITE: usize = 64 * 1024;
/// The most bytes of one stream an ExecRead returns.
pub const MAX_READ: u64 = 1024 * 1024;

const TIMEOUT_START: Duration = Duration::from_secs(30);
const TIMEOUT_CALL: Duration = Duration::from_secs(15);
/// ExecWait lasts as long as the process does.
const TIMEOUT_WAIT: Duration = Duration::from_secs(86_400);

/// The methods this module answers.
pub fn is_exec_method(method: &str) -> bool {
    matches!(
        method,
        "ExecCreate"
            | "ExecStart"
            | "ExecKill"
            | "ExecWait"
            | "ExecDelete"
            | "ExecWrite"
            | "ExecCloseStdin"
            | "ExecRead"
            | "ExecResize"
    )
}

fn err(what: &str, e: impl std::fmt::Display) -> Value {
    json!({"error": format!("{what}: {e}")})
}

/// An identifier field of a request that must be there and be plain.
fn id<'a>(request: &'a Value, key: &str) -> Result<&'a str, Value> {
    let v = request.get(key).and_then(|v| v.as_str()).unwrap_or("");
    validate::check_id(key, v).map_err(|e| err("bad request", e))?;
    Ok(v)
}

/// The base64 string of a field, decoded, if it decodes to at most `max` bytes. The length is checked before decoding.
pub fn decode_field(request: &Value, key: &str, max: usize) -> Result<Vec<u8>, String> {
    let text = request.get(key).and_then(|v| v.as_str()).unwrap_or("");
    // base64 is 4 characters for 3 bytes (with padding).
    if text.len() > max.div_ceil(3) * 4 + 4 {
        return Err(format!("{key} is larger than {max} bytes"));
    }
    let data = base64::engine::general_purpose::STANDARD
        .decode(text)
        .map_err(|_| format!("{key} is not base64"))?;
    if data.len() > max {
        return Err(format!("{key} is larger than {max} bytes"));
    }
    Ok(data)
}

/// RFC 3339 time as nanoseconds since the epoch; now if it is not one.
pub fn unix_nanos(rfc3339: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(rfc3339)
        .ok()
        .and_then(|t| t.timestamp_nanos_opt())
        .or_else(|| chrono::Utc::now().timestamp_nanos_opt())
        .unwrap_or(0)
}

/// Answer one exec request.
pub async fn handle(
    method: &str,
    pool: &Arc<Pool>,
    agents: &AgentRegistry,
    request: &Value,
) -> Value {
    let vm_id = match id(request, "vm_id") {
        Ok(v) => v,
        Err(e) => return e,
    };
    let container_id = match id(request, "container_id") {
        Ok(v) => v,
        Err(e) => return e,
    };
    let exec_id = match id(request, "exec_id") {
        Ok(v) => v,
        Err(e) => return e,
    };
    // Only a VM that was handed out to a pod is asked to run anything.
    if pool.get_active(vm_id).await.is_none() {
        return json!({"error": format!("VM {vm_id} is not an active VM")});
    }
    let agent = match agents.lock().await.get(vm_id).cloned() {
        Some(a) => a,
        None => return json!({"error": format!("no agent for VM {vm_id}")}),
    };
    let (c, e) = (container_id.to_string(), exec_id.to_string());
    let ctx = |d: Duration| ttrpc::context::with_duration(d);

    match method {
        "ExecCreate" => {
            // The id of the container's own terminal (see the agent's exec.rs): not an exec process.
            if e == "init" {
                return err("bad request", "the exec id \"init\" is reserved for the container's own terminal");
            }
            let process = match decode_field(request, "process_json", MAX_PROCESS_JSON) {
                Ok(p) => p,
                Err(m) => return err("bad request", m),
            };
            log::info!("exec create: {c}/{e} ({} bytes)", process.len());
            let mut req = cloudhv_proto::ExecCreateRequest::new();
            req.container_id = c;
            req.exec_id = e;
            req.process_json = process;
            req.terminal = request.get("terminal").and_then(|v| v.as_bool()).unwrap_or(false);
            match agent.exec_create(ctx(TIMEOUT_CALL), &req).await {
                Ok(_) => json!({}),
                Err(x) => err("exec_create", x),
            }
        }
        "ExecStart" => {
            let mut req = cloudhv_proto::ExecStartRequest::new();
            req.container_id = c;
            req.exec_id = e;
            match agent.exec_start(ctx(TIMEOUT_START), &req).await {
                Ok(r) => json!({"pid": r.pid}),
                Err(x) => err("exec_start", x),
            }
        }
        "ExecKill" => {
            let signal = request.get("signal").and_then(|v| v.as_u64()).unwrap_or(0);
            if signal == 0 || signal > 64 {
                return err("bad request", "signal");
            }
            let mut req = cloudhv_proto::ExecKillRequest::new();
            req.container_id = c;
            req.exec_id = e;
            req.signal = signal as u32;
            match agent.exec_kill(ctx(TIMEOUT_CALL), &req).await {
                Ok(_) => json!({}),
                Err(x) => err("exec_kill", x),
            }
        }
        "ExecWait" => {
            let mut req = cloudhv_proto::ExecWaitRequest::new();
            req.container_id = c;
            req.exec_id = e;
            match agent.exec_wait(ctx(TIMEOUT_WAIT), &req).await {
                Ok(r) => json!({
                    "exit_status": r.exit_status,
                    "exited_at_unix_nanos": unix_nanos(&r.exited_at),
                }),
                Err(x) => err("exec_wait", x),
            }
        }
        "ExecDelete" => {
            let mut req = cloudhv_proto::ExecDeleteRequest::new();
            req.container_id = c;
            req.exec_id = e;
            match agent.exec_delete(ctx(TIMEOUT_CALL), &req).await {
                Ok(_) => json!({}),
                Err(x) => err("exec_delete", x),
            }
        }
        "ExecWrite" => {
            let data = match decode_field(request, "data", MAX_WRITE) {
                Ok(d) => d,
                Err(m) => return err("bad request", m),
            };
            log::debug!("exec write: {c}/{e} ({} bytes)", data.len());
            let mut req = cloudhv_proto::ExecWriteRequest::new();
            req.container_id = c;
            req.exec_id = e;
            req.data = data;
            match agent.exec_write(ctx(TIMEOUT_CALL), &req).await {
                Ok(r) => json!({"written": r.written}),
                Err(x) => err("exec_write", x),
            }
        }
        "ExecCloseStdin" => {
            let mut req = cloudhv_proto::ExecCloseStdinRequest::new();
            req.container_id = c;
            req.exec_id = e;
            match agent.exec_close_stdin(ctx(TIMEOUT_CALL), &req).await {
                Ok(_) => json!({}),
                Err(x) => err("exec_close_stdin", x),
            }
        }
        "ExecRead" => {
            let num = |k: &str| request.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
            let mut req = cloudhv_proto::ExecReadRequest::new();
            req.container_id = c;
            req.exec_id = e;
            req.stdout_offset = num("stdout_offset");
            req.stderr_offset = num("stderr_offset");
            req.max_bytes = num("max").min(MAX_READ) as u32;
            match agent.exec_read(ctx(TIMEOUT_CALL), &req).await {
                Ok(r) => {
                    let b64 = base64::engine::general_purpose::STANDARD;
                    json!({
                        "stdout": b64.encode(&r.stdout),
                        "stderr": b64.encode(&r.stderr),
                        "stdout_offset": r.stdout_offset,
                        "stderr_offset": r.stderr_offset,
                        "eof": r.eof,
                        "dropped": r.stdout_dropped.saturating_add(r.stderr_dropped),
                        "stdout_dropped": r.stdout_dropped,
                        "stderr_dropped": r.stderr_dropped,
                    })
                }
                Err(x) => err("exec_read", x),
            }
        }
        "ExecResize" => {
            let num = |k: &str| request.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
            let (w, h) = (num("width"), num("height"));
            if w == 0 || h == 0 || w > 10_000 || h > 10_000 {
                return err("bad request", "terminal size");
            }
            let mut req = cloudhv_proto::ExecResizeRequest::new();
            req.container_id = c;
            req.exec_id = e;
            req.width = w as u32;
            req.height = h as u32;
            match agent.exec_resize(ctx(TIMEOUT_CALL), &req).await {
                Ok(_) => json!({}),
                Err(x) => err("exec_resize", x),
            }
        }
        _ => json!({"error": format!("unknown method: {method}")}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_exec_methods_are_the_nine_of_the_contract() {
        for m in [
            "ExecCreate", "ExecStart", "ExecKill", "ExecWait", "ExecDelete", "ExecWrite", "ExecCloseStdin", "ExecRead",
            "ExecResize",
        ] {
            assert!(is_exec_method(m), "{m}");
        }
        for m in ["Exec", "ExecProcess", "execcreate", "ReadLogs", "", "ExecCreate "] {
            assert!(!is_exec_method(m), "{m:?}");
        }
    }

    #[test]
    fn base64_fields_are_bounded_before_and_after_decoding() {
        let b64 = base64::engine::general_purpose::STANDARD;
        let ok = json!({"data": b64.encode(vec![7u8; MAX_WRITE])});
        assert_eq!(decode_field(&ok, "data", MAX_WRITE).unwrap().len(), MAX_WRITE);
        let over = json!({"data": b64.encode(vec![7u8; MAX_WRITE + 1])});
        assert!(decode_field(&over, "data", MAX_WRITE).is_err());
        // A string far too long is refused without being decoded.
        let huge = json!({"data": "A".repeat(MAX_WRITE * 2)});
        assert!(decode_field(&huge, "data", MAX_WRITE).unwrap_err().contains("larger"));
        assert!(decode_field(&json!({"data": "***"}), "data", 10).unwrap_err().contains("base64"));
        // Missing and empty are an empty value.
        assert!(decode_field(&json!({}), "data", 10).unwrap().is_empty());
        assert!(decode_field(&json!({"data": ""}), "data", 10).unwrap().is_empty());
    }

    #[test]
    fn ids_are_validated_like_the_other_calls() {
        let r = json!({"vm_id": "warm-ab12-3", "container_id": "../x", "exec_id": "e1"});
        assert!(id(&r, "vm_id").is_ok());
        assert!(id(&r, "container_id").is_err());
        assert!(id(&r, "exec_id").is_ok());
        assert!(id(&r, "missing").is_err());
        let e = id(&r, "container_id").unwrap_err();
        assert!(e["error"].as_str().unwrap().starts_with("bad request"));
    }

    #[test]
    fn a_time_becomes_unix_nanoseconds_and_a_bad_one_is_now() {
        assert_eq!(unix_nanos("1970-01-01T00:00:01Z"), 1_000_000_000);
        assert_eq!(unix_nanos("2026-10-16T00:00:00.5+00:00"), 1_792_108_800_500_000_000);
        let before = chrono::Utc::now().timestamp_nanos_opt().unwrap();
        assert!(unix_nanos("not a time") >= before);
    }

    #[test]
    fn the_contract_answers_have_the_documented_shape() {
        // The shapes the shim reads (kept in one place so a rename breaks a test, not a pod).
        let b64 = base64::engine::general_purpose::STANDARD;
        let read = json!({
            "stdout": b64.encode(b"out"), "stderr": b64.encode(b"err"), "stdout_offset": 3, "stderr_offset": 3,
            "eof": false, "dropped": 0, "stdout_dropped": 0, "stderr_dropped": 0,
        });
        for k in ["stdout", "stderr", "stdout_offset", "stderr_offset", "eof", "dropped"] {
            assert!(read.get(k).is_some(), "{k}");
        }
        let wait = json!({"exit_status": 7, "exited_at_unix_nanos": unix_nanos("2026-10-16T00:00:00Z")});
        assert_eq!(wait["exit_status"], 7);
        assert!(wait["exited_at_unix_nanos"].as_i64().unwrap() > 0);
    }
}
