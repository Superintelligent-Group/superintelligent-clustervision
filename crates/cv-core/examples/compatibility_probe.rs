//! Private compatibility fixture: use the library's real public serde types, never a copied codec.
use cv_core::Message;
use serde_json::{json, Value};

fn main() {
    let rich = json!({
        "id":"m", "parent_id":"p", "role":"assistant", "kind":"reply", "origin":"model",
        "timestamp":"2026-01-01T00:00:00Z", "model":"fixture", "extra":{"claude":{"opaque":true}},
        "usage":{"input_tokens":1,"output_tokens":2,"cache_read_tokens":3,"cache_creation_tokens":4,
                 "reasoning_tokens":5,"cost_usd":0.5},
        "content":[
            {"type":"text","text":"hello"},
            {"type":"thinking","text":"thought","signature":"s","encrypted":"e","redacted":true},
            {"type":"tool_use","id":"t","name":"test","input":{"n":1},"namespace":"ns"},
            {"type":"tool_result","tool_use_id":"t","content":"ok","is_error":true,
             "tool_name":"test","status":"done","details":{"opaque":true}},
            {"type":"image","media_type":"image/png","data_ref":"opaque"},
            {"type":"file","mime":"text/plain","path":"fixture.txt","source":"opaque"}
        ]
    });
    let messages: Vec<Value> = [rich, json!({"role":"user"})].into_iter().map(|v| {
        let message: Message = serde_json::from_value(v).expect("released Message input remains readable");
        serde_json::to_value(message).expect("serialize real Message")
    }).collect();
    println!("{}", json!(messages));
}
