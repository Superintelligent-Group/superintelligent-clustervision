//! Checked per-block projection: retained text stays borrowed, discarded text is never copied.
use super::{thinking_fate_with_presence, thinking_placeholder, ThinkingFate, ThinkingMode};
use crate::{lazy::Resolver, Block, Harness, Text};
use anyhow::Result;
use std::borrow::Cow;

#[derive(Debug)]
struct Nonblank;
impl std::fmt::Display for Nonblank {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("nonblank text found")
    }
}
impl std::error::Error for Nonblank {}
fn nonblank(text: &Text, resolver: &Resolver) -> Result<bool> {
    if let Some(inline) = text.inline_str() {
        return Ok(!inline.trim_start().is_empty());
    }
    match text.try_for_each_chunk(resolver, 64 * 1024, |chunk| {
        if !chunk.trim_start().is_empty() {
            return Err(Nonblank.into());
        }
        Ok(())
    }) {
        Ok(()) => Ok(false),
        Err(error) if error.is::<Nonblank>() => Ok(true),
        Err(error) => Err(error),
    }
}
pub(crate) fn fate(block: &Block, target: Harness, mode: ThinkingMode, resolver: &Resolver) -> Result<ThinkingFate> {
    let Block::Thinking {
        text,
        signature,
        encrypted,
        redacted,
    } = block
    else {
        return Ok(ThinkingFate::Keep);
    };
    // Native retains the existing emitter codec policy, including its empty-block handling.
    if mode == ThinkingMode::Native {
        return Ok(ThinkingFate::Keep);
    }
    if mode == ThinkingMode::Drop {
        return Ok(ThinkingFate::Drop);
    }
    let decide = |present| {
        thinking_fate_with_presence(
            target,
            mode,
            present,
            signature.as_deref(),
            encrypted.as_deref(),
            *redacted,
        )
    };
    if decide(false) == ThinkingFate::Keep {
        return Ok(ThinkingFate::Keep);
    }
    Ok(decide(nonblank(text, resolver)?))
}
pub(crate) fn text<'a>(
    block: &'a Block,
    target: Harness,
    mode: ThinkingMode,
    resolver: &Resolver,
) -> Result<Option<Cow<'a, Text>>> {
    if let Block::Text { text } = block {
        return Ok(Some(Cow::Borrowed(text)));
    }
    Ok(text_with_fate(block, fate(block, target, mode, resolver)?))
}
pub(crate) fn text_with_fate(block: &Block, fate: ThinkingFate) -> Option<Cow<'_, Text>> {
    match fate {
        ThinkingFate::AsText => {
            let Block::Thinking { text, .. } = block else {
                unreachable!()
            };
            Some(Cow::Borrowed(text))
        }
        ThinkingFate::Placeholder => {
            let Block::Thinking {
                signature,
                encrypted,
                redacted,
                ..
            } = block
            else {
                unreachable!()
            };
            Some(Cow::Owned(
                thinking_placeholder(signature.as_deref(), encrypted.as_deref(), *redacted).into(),
            ))
        }
        _ => None,
    }
}
