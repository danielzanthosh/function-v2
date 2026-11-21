use crate::PlatformError;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Mutex, OnceLock};

fn request_message(id: u64, method: &str, params: Value) -> Value {
    json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params})
}

#[derive(Debug, PartialEq, Eq)]
struct RpcError {
    code: i64,
    message: String,
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "Native helper error {}: {}", self.code, self.message)
    }
}

fn response_result(response: &Value, id: u64) -> Result<Option<Value>, RpcError> {
    if response.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
        || response.get("id").and_then(Value::as_u64) != Some(id)
    {
        return Ok(None);
    }
    if let Some(error) = response.get("error") {
        return Err(RpcError {
            code: error.get("code").and_then(Value::as_i64).unwrap_or(-32000),
            message: error.get("message").and_then(Value::as_str).unwrap_or("Native helper returned an unspecified error").to_string(),
        });
    }
    response.get("result").cloned().map(Some).ok_or_else(|| RpcError { code: -32600, message: "Native helper response has no result".into() })
}

struct HelperProcess {
    child: Child,
    stdin: BufWriter<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl HelperProcess {
    fn spawn() -> Result<Self, PlatformError> {
        let helper_path = crate::computer::macos_capture_helper();
        let mut child = Command::new(&helper_path).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn()
            .map_err(|error| PlatformError::SystemApi(format!("Could not launch native helper at {}: {error}", helper_path.display())))?;
        let stdin = child.stdin.take().ok_or_else(|| PlatformError::SystemApi("Native helper stdin unavailable".into()))?;
        let stdout = child.stdout.take().ok_or_else(|| PlatformError::SystemApi("Native helper stdout unavailable".into()))?;
        Ok(Self { child, stdin: BufWriter::new(stdin), stdout: BufReader::new(stdout), next_id: 1 })
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value, PlatformError> {
        if let Some(status) = self.child.try_wait().map_err(|error| PlatformError::SystemApi(error.to_string()))? {
            return Err(PlatformError::SystemApi(format!("Native helper exited with {status}")));
        }
        let id = self.next_id;
        self.next_id += 1;
        serde_json::to_writer(&mut self.stdin, &request_message(id, method, params)).map_err(|error| PlatformError::SystemApi(error.to_string()))?;
        self.stdin.write_all(b"\n").and_then(|_| self.stdin.flush()).map_err(|error| PlatformError::SystemApi(format!("Could not write to native helper: {error}")))?;
        loop {
            let mut line = String::new();
            if self.stdout.read_line(&mut line).map_err(|error| PlatformError::SystemApi(format!("Could not read from native helper: {error}")))? == 0 {
                return Err(PlatformError::SystemApi("Native helper closed its response stream".into()));
            }
            let response: Value = serde_json::from_str(&line).map_err(|error| PlatformError::SystemApi(format!("Invalid JSON-RPC response: {error}")))?;
            match response_result(&response, id) {
                Ok(Some(result)) => return Ok(result),
                Ok(None) => continue,
                Err(error) => return Err(PlatformError::SystemApi(error.to_string())),
            }
        }
    }
}

impl Drop for HelperProcess {
    fn drop(&mut self) {
        let _ = self.request("shutdown", json!({}));
        for _ in 0..100 {
            if self.child.try_wait().ok().flatten().is_some() { return; }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

static HELPER: OnceLock<Mutex<Option<HelperProcess>>> = OnceLock::new();

pub(crate) fn call(method: &str, params: Value) -> Result<Value, PlatformError> {
    let mut helper = HELPER.get_or_init(|| Mutex::new(None)).lock().map_err(|_| PlatformError::SystemApi("Native helper lock was poisoned".into()))?;
    if helper.is_none() { *helper = Some(HelperProcess::spawn()?); }
    match helper.as_mut().expect("helper initialized").request(method, params) {
        Ok(result) => Ok(result),
        Err(error) => {
            helper.take();
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_uses_json_rpc_2_envelope() {
        assert_eq!(request_message(7, "listWindows", json!({})), json!({"jsonrpc":"2.0", "id":7, "method":"listWindows", "params":{}}));
    }

    #[test]
    fn response_errors_keep_the_native_message() {
        let response = json!({"jsonrpc":"2.0", "id":9, "error":{"code":-32001,"message":"Permission denied"}});
        assert_eq!(response_result(&response, 9), Err(RpcError { code: -32001, message: "Permission denied".into() }));
    }

    #[test]
    fn unrelated_response_ids_are_ignored() {
        let response = json!({"jsonrpc":"2.0", "id":8, "result":{}});
        assert_eq!(response_result(&response, 9), Ok(None));
    }
}
