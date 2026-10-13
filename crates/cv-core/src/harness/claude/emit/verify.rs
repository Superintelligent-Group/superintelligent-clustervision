//! Resumability facts from strict checked JSONL views; no retained transcript payloads.
use super::*;
use crate::{
    harness::store::jsonl::ranges,
    json::{JsonSource, JsonView},
};
use std::collections::HashSet;
#[derive(Default)]
struct Facts {
    parent: Option<String>,
    calls: Vec<String>,
    side: Option<bool>,
    agent: Option<String>,
}
fn string(view: Option<JsonView<'_>>) -> Result<Option<String>> {
    let Some(view) = view else { return Ok(None) };
    if !view.is_string()? {
        return Ok(None);
    }
    Ok(view.resolve()?.as_str().map(str::to_owned))
}
pub(super) fn verify(path: &Path, agent: Option<&str>) -> Result<()> {
    let source = JsonSource::capture(path)?;
    let file = fs::File::open(path)?;
    let framed = ranges::records(
        &file,
        ranges::Range {
            offset: 0,
            len: source.len(),
        },
    )?;
    let (mut ids, mut answered) = (HashSet::new(), HashSet::new());
    let mut facts = Vec::new();
    for row in framed {
        let (raw, _) = row?;
        if ranges::trim(&file, raw)?.len == 0 {
            continue;
        }
        let fact = source
            .read_range_value(raw.offset, raw.len, |view| {
                if let Some(id) = string(view.get("uuid")?)? {
                    ids.insert(id);
                }
                let mut fact = Facts {
                    parent: string(view.get("parentUuid")?)?,
                    side: view.get("isSidechain")?.map(|v| v.as_bool()).transpose()?.flatten(),
                    agent: string(view.get("agentId")?)?,
                    calls: Vec::new(),
                };
                if let Some(array) = view
                    .get("message")?
                    .map(|v| v.get("content"))
                    .transpose()?
                    .flatten()
                    .map(|v| v.array())
                    .transpose()?
                    .flatten()
                {
                    for block in array {
                        let block = block?;
                        let ty = string(block.get("type")?)?;
                        match ty.as_deref() {
                            Some("tool_use") => fact.calls.push(string(block.get("id")?)?.unwrap_or_default()),
                            Some("tool_result") => {
                                if let Some(id) = string(block.get("tool_use_id")?)? {
                                    answered.insert(id);
                                }
                            }
                            _ => {}
                        }
                    }
                }
                Ok(fact)
            })
            .with_context(|| format!("{}: not a JSON record", path.display()))?;
        facts.push(fact);
    }
    source.check()?;
    for (i, fact) in facts.iter().enumerate() {
        let n = i + 1;
        if let Some(parent) = &fact.parent {
            if !ids.contains(parent) {
                anyhow::bail!(
                    "{}:{n}: parentUuid {parent} names no record in the file",
                    path.display()
                );
            }
        }
        for id in &fact.calls {
            if !answered.contains(id) {
                anyhow::bail!("{}:{n}: tool_use {id} has no tool_result", path.display());
            }
        }
        match agent {
            None if fact.side == Some(true) || fact.agent.is_some() => anyhow::bail!(
                "{}:{n}: a main-thread session carries a sidechain marker",
                path.display()
            ),
            Some(a) if fact.side != Some(true) || fact.agent.as_deref() != Some(a) => anyhow::bail!(
                "{}:{n}: a sub-agent record must carry isSidechain:true and agentId {a:?}",
                path.display()
            ),
            _ => {}
        }
    }
    Ok(())
}
