//! Isolated production Codex reader + discard/canonical-JSON consumer. Source is read-only.
//! Usage: codex_selected_probe SOURCE discard|json|digest [RUNS]; use scratch CLUSTERVISION_HOME.
use anyhow::{Context, Result};
use cv_core::{
    stream::{Flow, MessageSink, ParseOptions},
    Harness, Message, Resolver, SessionRef,
};
use std::{
    io::{self, Write},
    path::PathBuf,
};
#[cfg(feature = "probe-heap")]
#[global_allocator]
static ALLOCATOR: cv_testkit::heap::Allocator = cv_testkit::heap::Allocator;
struct Consumer {
    resolver: Resolver,
    write: bool,
    hash: Option<cv_core::digest::Sha256>,
    bytes: u64,
    messages: usize,
    failure: Option<anyhow::Error>,
}
impl Write for Consumer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if let Some(hash) = &mut self.hash {
            hash.update(bytes);
        }
        self.bytes += bytes.len() as u64;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
struct CountWriter<'a> {
    hash: &'a mut Option<cv_core::digest::Sha256>,
    bytes: &'a mut u64,
}
impl Write for CountWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if let Some(hash) = self.hash {
            hash.update(bytes);
        }
        *self.bytes += bytes.len() as u64;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl MessageSink for Consumer {
    fn message(&mut self, message: Message) -> Flow {
        self.messages += 1;
        if self.write {
            let mut writer = CountWriter {
                hash: &mut self.hash,
                bytes: &mut self.bytes,
            };
            if let Err(error) = message.write_json(&mut writer, &self.resolver) {
                self.failure = Some(error);
                return Flow::Stop;
            }
        }
        Flow::Continue
    }
    fn take_error(&mut self) -> Option<anyhow::Error> {
        self.failure.take()
    }
}
fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    anyhow::ensure!(
        args.len() >= 2,
        "codex_selected_probe SOURCE discard|json|digest [RUNS]"
    );
    std::env::var_os("CLUSTERVISION_HOME").context("set scratch CLUSTERVISION_HOME")?;
    let path = std::fs::canonicalize(PathBuf::from(&args[0]))?;
    let reference = SessionRef::new("probe", Harness::Codex, &path);
    let adapter = cv_core::harness::codex::Codex::detect();
    let mut consumer = Consumer {
        resolver: Resolver::new(Some(path)),
        write: args[1] != "discard",
        hash: (args[1] == "digest").then(cv_core::digest::Sha256::new),
        bytes: 0,
        messages: 0,
        failure: None,
    };
    let runs: usize = args.get(2).map(|s| s.parse()).transpose()?.unwrap_or(1);
    #[cfg(feature = "probe-heap")]
    let baseline = cv_testkit::heap::begin();
    let started = std::time::Instant::now();
    for _ in 0..runs {
        let session = cv_core::harness::Adapter::stream(
            &adapter,
            &reference,
            &ParseOptions {
                spans: true,
                ..ParseOptions::full()
            },
            &mut consumer,
        )?;
        if consumer.write {
            serde_json::to_writer(&mut consumer, &session)?;
        }
    }
    #[cfg(feature = "probe-heap")]
    eprintln!("{}", serde_json::to_string(&cv_testkit::heap::end(baseline))?);
    println!(
        "{}",
        serde_json::json!({"seconds":started.elapsed().as_secs_f64(),"runs":runs,"messages":consumer.messages,"json_bytes":consumer.bytes,"sha256":consumer.hash.map(|hash|hash.finish_hex())})
    );
    Ok(())
}
