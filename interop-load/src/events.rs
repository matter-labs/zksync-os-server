use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use serde_json::{Value, json};
use uuid::Uuid;

pub struct EventWriter {
    run_id: Uuid,
    writer: BufWriter<File>,
}

impl EventWriter {
    pub fn create(output_dir: &Path, run_id: Uuid) -> anyhow::Result<Self> {
        std::fs::create_dir_all(output_dir)
            .with_context(|| format!("failed to create {}", output_dir.display()))?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(output_dir.join("events.jsonl"))
            .with_context(|| {
                format!(
                    "failed to open {}",
                    output_dir.join("events.jsonl").display()
                )
            })?;
        Ok(Self {
            run_id,
            writer: BufWriter::new(file),
        })
    }

    pub fn emit(&mut self, event: &str, value: Value) -> anyhow::Result<()> {
        let Value::Object(mut object) = value else {
            anyhow::bail!("event payload for {event} must be a JSON object");
        };
        object.insert("ts_ms".to_string(), json!(now_ms()));
        object.insert("event".to_string(), json!(event));
        object.insert("run_id".to_string(), json!(self.run_id));

        serde_json::to_writer(&mut self.writer, &Value::Object(object))?;
        self.writer.write_all(b"\n")?;
        Ok(())
    }

    pub fn flush(&mut self) -> anyhow::Result<()> {
        self.writer.flush()?;
        Ok(())
    }
}

impl Drop for EventWriter {
    fn drop(&mut self) {
        // Best-effort flush so a panic doesn't lose up to a second of buffered
        // bundle events.
        let _ = self.writer.flush();
    }
}

pub fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock must be after unix epoch")
        .as_millis()
}
