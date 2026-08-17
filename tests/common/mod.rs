use std::fs;
use std::path::Path;

pub fn force_checkpoint_now(state: &Path, session: &str) {
    let path = state.join(format!("{session}.json"));
    let mut record: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    record["next_checkpoint_at_ms"] = (record["created_at_ms"].as_u64().unwrap() + 1).into();
    fs::write(path, serde_json::to_vec_pretty(&record).unwrap()).unwrap();
}
