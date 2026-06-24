//! Midstate pool Stratum wire protocol (newline-delimited JSON over plain TCP).
//!
//! This is JSON-RPC-ish, NOT full Stratum: no `set_difficulty`, no extranonce,
//! no version-rolling. Handshake = `mining.authorize [address, worker]`; the pool
//! then pushes `mining.notify [job_id, midstate_hex, batch_template]`. The miner
//! only needs `job_id` + `midstate_hex` to grind; it submits
//! `mining.submit [address, job_id, nonce]` with nonce as a **plain JSON integer**.
//! Share/network targets are checked pool-side and never sent on the wire.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Outbound request (authorize / submit / subscribe). Serialized compact + `\n`-framed.
#[derive(Serialize)]
pub struct RpcRequest<'a> {
    pub id: u64,
    pub method: &'a str,
    pub params: Value,
}

/// One inbound line — may be a `mining.notify` (method+params, id null) OR a
/// response (id + result/error). Untagged so a single parse handles both.
#[derive(Deserialize, Debug, Default)]
pub struct Incoming {
    #[serde(default)]
    pub id: Option<u64>,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub params: Option<Vec<Value>>,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<Value>,
}

/// A job parsed from a `mining.notify`. `batch_template` (params[2]) is dropped —
/// the miner does not need it to mine or submit.
#[derive(Clone, Debug, PartialEq)]
pub struct Job {
    pub job_id: u64,
    pub midstate: [u8; 32],
}

/// High-level classification of one inbound message for the client loop.
#[derive(Debug, PartialEq)]
pub enum Event {
    Job(Job),
    AuthAck(bool),
    SubmitAck { accepted: bool, error: Option<String> },
    Other,
}

/// Conventional request ids (the pool echoes whatever id we send).
pub const ID_AUTHORIZE: u64 = 1;
pub const ID_SUBMIT: u64 = 2;

fn parse_midstate(hex_str: &str) -> Option<[u8; 32]> {
    let bytes = hex::decode(hex_str).ok()?;
    if bytes.len() != 32 {
        return None;
    }
    let mut m = [0u8; 32];
    m.copy_from_slice(&bytes);
    Some(m)
}

/// Classify one inbound message. Notifies become `Job`; responses are matched by
/// the id we sent (authorize=1, submit=2) with `result` a bool.
pub fn classify(msg: Incoming) -> Event {
    if msg.method.as_deref() == Some("mining.notify") {
        if let Some(params) = &msg.params {
            if params.len() >= 2 {
                if let (Some(job_id), Some(mid_hex)) = (params[0].as_u64(), params[1].as_str()) {
                    if let Some(midstate) = parse_midstate(mid_hex) {
                        return Event::Job(Job { job_id, midstate });
                    }
                }
            }
        }
        return Event::Other;
    }
    match msg.id {
        Some(ID_AUTHORIZE) => Event::AuthAck(
            msg.result
                .as_ref()
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        ),
        Some(ID_SUBMIT) => {
            let accepted = msg
                .result
                .as_ref()
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let error = msg
                .error
                .as_ref()
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            Event::SubmitAck { accepted, error }
        }
        _ => Event::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(line: &str) -> Incoming {
        serde_json::from_str(line).unwrap()
    }

    #[test]
    fn classifies_notify_into_job() {
        let mid = "00".repeat(32); // 64 hex zeros
        let line = format!(
            r#"{{"id":null,"method":"mining.notify","params":[42,"{}",{{"timestamp":1}}]}}"#,
            mid
        );
        match classify(parse(&line)) {
            Event::Job(j) => {
                assert_eq!(j.job_id, 42);
                assert_eq!(j.midstate, [0u8; 32]);
            }
            other => panic!("expected Job, got {:?}", other),
        }
    }

    #[test]
    fn authorize_ack() {
        assert_eq!(
            classify(parse(r#"{"id":1,"result":true,"error":null}"#)),
            Event::AuthAck(true)
        );
        assert_eq!(
            classify(parse(r#"{"id":1,"result":false,"error":"Invalid Address"}"#)),
            Event::AuthAck(false)
        );
    }

    #[test]
    fn submit_ack_accept_and_reject() {
        assert_eq!(
            classify(parse(r#"{"id":2,"result":true,"error":null}"#)),
            Event::SubmitAck {
                accepted: true,
                error: None
            }
        );
        assert_eq!(
            classify(parse(r#"{"id":2,"result":false,"error":"Low difficulty"}"#)),
            Event::SubmitAck {
                accepted: false,
                error: Some("Low difficulty".to_string())
            }
        );
    }

    #[test]
    fn malformed_notify_is_other_not_panic() {
        // Short params, bad hex, wrong types — must never panic, just Other.
        assert_eq!(
            classify(parse(r#"{"id":null,"method":"mining.notify","params":[1]}"#)),
            Event::Other
        );
        assert_eq!(
            classify(parse(r#"{"id":null,"method":"mining.notify","params":[1,"zz"]}"#)),
            Event::Other
        );
        assert_eq!(classify(parse(r#"{"id":99,"result":true}"#)), Event::Other);
    }
}
