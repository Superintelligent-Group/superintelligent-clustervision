//! Guard actual publication of captured JSON arguments. Tiny/owned messages do no filesystem work.
use super::*;
use crate::json::JsonSource;

pub(super) struct SourceSink<'a> {
    source: Option<&'a JsonSource>,
    inner: &'a mut dyn MessageSink,
    error: Option<anyhow::Error>,
    failed: bool,
    published: bool,
}
impl<'a> SourceSink<'a> {
    pub fn new(source: Option<&'a JsonSource>, inner: &'a mut dyn MessageSink) -> Self {
        Self {
            source,
            inner,
            error: None,
            failed: false,
            published: false,
        }
    }
    pub fn finish(&mut self) -> Result<bool> {
        match self.take_error() {
            Some(error) => Err(error),
            None => Ok(self.published),
        }
    }
    fn check(&mut self) -> bool {
        if let Some(source) = self.source {
            if let Err(error) = source.check() {
                self.failed = true;
                self.error.get_or_insert(error);
                return false;
            }
        }
        true
    }
}
impl MessageSink for SourceSink<'_> {
    fn message(&mut self, m: Message) -> Flow {
        if self.failed {
            return Flow::Stop;
        }
        let sourced = m.content.iter().any(|b| {
            matches!(
                b,
                Block::ToolUse {
                    input: crate::Json::Source(_),
                    ..
                }
            )
        });
        if sourced && !self.check() {
            return Flow::Stop;
        }
        self.published |= sourced;
        let flow = self.inner.message(m);
        if sourced && !self.check() {
            return Flow::Stop;
        }
        flow
    }
    fn take_error(&mut self) -> Option<anyhow::Error> {
        self.error.take().or_else(|| self.inner.take_error()).or_else(|| {
            self.failed
                .then(|| anyhow::anyhow!("source publication has already failed"))
        })
    }
    fn meta(&mut self, s: &Session) {
        self.inner.meta(s);
    }
    fn dependency(&mut self, path: &Path) {
        self.inner.dependency(path);
    }
    fn wants_checkpoint(&mut self, offset: u64) -> bool {
        self.inner.wants_checkpoint(offset)
    }
    fn checkpoint(&mut self, cp: &mut crate::stream::Checkpoint) {
        self.inner.checkpoint(cp);
    }
}
