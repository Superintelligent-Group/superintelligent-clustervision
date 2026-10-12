//! The real [`ExtFacts`] resolver: answers the query calculus's external predicates against one
//! session, and filters a ref list with the full two-phase evaluation.
//!
//! Every fact source here is cv-core's own — the events extractor, the sub-agent forest, workflow
//! state, the compaction scan — with one exception: `text:` needs a full-text index, which lives
//! in cv-search (and cv-search depends on cv-core, so the dependency cannot point that way). So
//! `text:` arrives pre-resolved as a [`TextSets`], built by whichever door is asking: `cv -q` and
//! `cv serve`'s `/api/stats?q=` both hand in the tantivy index, and a caller with no index hands in
//! nothing and `text:` matches nothing. That keeps one implementation of the calculus behind both
//! doors, which is the point — a `-q` that means something different on the daemon than on the
//! CLI is a silent wrong answer, not a missing feature.

use crate::events::{self, Event};
use crate::ir::{Block, Message, Session, SessionRef};
use crate::query::{ExtFacts, Facts, FieldId, SessionQuery, Tri};
use std::cell::OnceCell;
use std::collections::{HashMap, HashSet};

/// Pre-resolved full-text results: each distinct `text:` needle → the set of session ids that match
/// it. Computed once before a scan so per-session evaluation is a set lookup.
#[derive(Debug, Default, Clone)]
pub struct TextSets(HashMap<String, HashSet<String>>);

impl TextSets {
    /// An empty set (no `text:` needles) — for callers with no query.
    pub fn empty() -> TextSets {
        TextSets(HashMap::new())
    }

    /// Resolve every distinct `text:` needle in `query` with `search`, which answers "which session
    /// ids match this needle". A caller with no index passes a closure returning an empty set, and
    /// every `text:` needle then matches nothing (the CLI warns once when it does this).
    pub fn resolve(query: &SessionQuery, search: impl Fn(&str) -> HashSet<String>) -> TextSets {
        let mut map = HashMap::new();
        for n in query.needles(FieldId::Text) {
            let ids = search(&n);
            map.insert(n, ids);
        }
        TextSets(map)
    }

    /// Whether `id` matched `needle` (already lowercased by the parser, matching the map keys).
    pub fn contains(&self, needle: &str, id: &str) -> bool {
        self.0.get(needle).is_some_and(|s| s.contains(id))
    }
}

/// The external-predicate resolver for one parsed session: `text:` from the precomputed
/// [`TextSets`], `tool:`/`touched:`/`has:` from the session's events (extracted in-memory), and the
/// forest-tier predicates (`subtool:`/`agent:`/`agents`/`workflow(s)`/`compactions`) from the
/// sub-agent forest / workflow state / compaction scan. Every expensive walk is memoized so it
/// runs at most once.
pub struct SessionFacts<'a> {
    r: &'a SessionRef,
    session: &'a Session,
    text_sets: &'a TextSets,
    events: OnceCell<Vec<Event>>,
    forest: OnceCell<crate::tools::ForestTools>,
    tree: OnceCell<Vec<crate::SubagentInfo>>,
    workflows: OnceCell<Vec<crate::Workflow>>,
    compactions: OnceCell<usize>,
    affordances: OnceCell<HashSet<crate::affordance::Kind>>,
    reduced: Option<&'a ReducedFacts>,
}

impl<'a> SessionFacts<'a> {
    pub fn new(r: &'a SessionRef, session: &'a Session, text_sets: &'a TextSets) -> Self {
        SessionFacts {
            r,
            session,
            text_sets,
            events: OnceCell::new(),
            forest: OnceCell::new(),
            tree: OnceCell::new(),
            workflows: OnceCell::new(),
            compactions: OnceCell::new(),
            affordances: OnceCell::new(),
            reduced: None,
        }
    }

    fn events(&self) -> &[Event] {
        self.events.get_or_init(|| {
            let cwd = self.session.cwd.as_deref();
            self.session
                .messages
                .iter()
                .enumerate()
                .flat_map(|(i, m)| events::extract(m, i, cwd))
                .collect()
        })
    }

    /// Per-agent tool histograms across the orchestrator + forest (parses every sub-agent — pricey).
    fn forest(&self) -> &crate::tools::ForestTools {
        self.forest
            .get_or_init(|| crate::tools::forest_tools(self.r).unwrap_or(crate::tools::ForestTools { agents: vec![] }))
    }

    /// The sub-agent forest as lightweight infos (type + ref + journaled outcome).
    fn tree(&self) -> &[crate::SubagentInfo] {
        self.tree.get_or_init(|| crate::subagent_tree_of(self.r))
    }

    fn workflows(&self) -> &[crate::Workflow] {
        self.workflows.get_or_init(|| crate::workflows_of(self.r))
    }

    /// The affordance kinds this session shows, transcript and side store.
    fn affordance_kinds(&self) -> &HashSet<crate::affordance::Kind> {
        self.affordances.get_or_init(|| {
            let mut rows = crate::affordance::of_session(self.session);
            if let Some(a) = crate::harness::for_harness(self.r.harness) {
                rows.extend(a.store_affordances(self.r));
            }
            rows.into_iter().map(|a| a.kind).collect()
        })
    }

    fn compaction_count(&self) -> usize {
        // Keep the detector's extra-enabled read: some legacy lean readers tolerate malformed
        // ignored sidecars that this pass rejects. Counting lazy IR kinds changes those results.
        *self
            .compactions
            .get_or_init(|| crate::compaction::detect(self.r, false).map(|c| c.len()).unwrap_or(0))
    }
}

impl ExtFacts for SessionFacts<'_> {
    fn model(&self, needle: &str, exact: bool) -> Tri {
        self.reduced
            .and_then(|f| f.models.get(needle))
            .map_or(Tri::Unknown, |m| Tri::of(if exact { m.exact } else { m.contains }))
    }

    fn text(&self, needle: &str) -> Tri {
        // The id key is the session id; `needle` is already lowercased by the parser, matching the
        // map keys we inserted under.
        Tri::of(self.text_sets.contains(needle, &self.session.id))
    }

    fn tool(&self, name: &str) -> Tri {
        if let Some(facts) = self.reduced {
            return Tri::of(facts.tools.contains(name));
        }
        Tri::of(
            self.events()
                .iter()
                .any(|e| e.tool.as_deref().is_some_and(|t| t.to_lowercase().contains(name))),
        )
    }

    fn touched(&self, path: &str) -> Tri {
        if let Some(facts) = self.reduced {
            return Tri::of(facts.targets.contains(path));
        }
        Tri::of(self.events().iter().any(|e| {
            matches!(e.kind, "file_edit" | "file_read")
                && e.target.as_deref().is_some_and(|t| t.to_lowercase().contains(path))
        }))
    }

    fn has(&self, flag: &str) -> Tri {
        Tri::of(match flag {
            "errors" => self
                .reduced
                .map_or_else(|| self.events().iter().any(|e| e.kind == "error"), |f| f.errors),
            "tools" => self
                .reduced
                .map_or_else(|| self.events().iter().any(|e| e.tool.is_some()), |f| f.has_tools),
            "images" => self.reduced.map_or_else(
                || {
                    self.session
                        .messages
                        .iter()
                        .any(|m| m.content.iter().any(|b| matches!(b, Block::Image { .. })))
                },
                |f| f.images,
            ),
            "subagents" => {
                self.session.harness == crate::ir::Harness::Claude
                    && self
                        .session
                        .source_path
                        .as_deref()
                        .is_some_and(|p| !crate::harness::claude::subagent_refs(p).is_empty())
            }
            "compacted" => self.compaction_count() > 0,
            "workflows" => !self.workflows().is_empty(),
            other => crate::affordance::Kind::parse(other).is_some_and(|k| {
                self.reduced.map_or_else(
                    || self.affordance_kinds().contains(&k),
                    |f| {
                        f.counts
                            .as_ref()
                            .is_some_and(|counts| counts.get(&k).is_some_and(|n| *n > 0))
                    },
                )
            }),
        })
    }

    fn subtool(&self, name: &str) -> Tri {
        // A tool used by any *sub-agent* (skip the orchestrator sentinel).
        Tri::of(self.forest().agents.iter().any(|a| {
            a.agent != crate::tools::ORCHESTRATOR && a.histogram.tools.keys().any(|t| t.to_lowercase().contains(name))
        }))
    }

    fn agent_type(&self, needle: &str) -> Tri {
        Tri::of(self.tree().iter().any(|info| {
            info.agent_type
                .as_deref()
                .is_some_and(|t| t.to_lowercase().contains(needle))
        }))
    }

    fn workflow(&self, needle: &str) -> Tri {
        Tri::of(self.workflows().iter().any(|w| {
            w.run_id.to_lowercase().contains(needle)
                || w.name.as_deref().is_some_and(|n| n.to_lowercase().contains(needle))
        }))
    }

    fn count(&self, field: FieldId) -> Option<u64> {
        Some(match field {
            FieldId::Agents => self.tree().len() as u64,
            FieldId::Workflows => self.workflows().len() as u64,
            FieldId::Compactions => self.compaction_count() as u64,
            _ => return None,
        })
    }
}

/// Full evaluation of `query` against a parsed session + its ref, using a [`SessionFacts`] resolver.
pub fn matches_full(query: &SessionQuery, r: &SessionRef, session: &Session, text_sets: &TextSets) -> bool {
    let facts = SessionFacts::new(r, session, text_sets);
    query.matches(&Facts {
        r,
        session: Some(session),
        ext: &facts,
    })
}

/// The transcript facts the query actually consults. Repeated calls and rich payloads contribute
/// no retained messages or event rows. Model/tool and matched-path state scale with query needles.
/// Unmatched relative paths still scale with distinct source targets until EOF because the
/// whole-session evaluator resolves every target against the final (possibly late) cwd.
#[derive(Default)]
struct ReducedFacts {
    models: HashMap<String, ModelMatch>,
    tools: HashSet<String>,
    targets: HashSet<String>,
    errors: bool,
    has_tools: bool,
    images: bool,
    counts: Option<crate::affordance::Counts>,
}

#[derive(Default)]
struct ModelMatch {
    contains: bool,
    exact: bool,
}

impl ReducedFacts {
    fn note_model(&mut self, model: &str) {
        if !self.models.is_empty() {
            let model = model.to_lowercase();
            for (needle, matched) in &mut self.models {
                matched.contains |= model.contains(needle.as_str());
                matched.exact |= model == *needle;
            }
        }
    }
}

struct FactSink {
    facts: ReducedFacts,
    tools: Vec<String>,
    targets: Vec<String>,
    pending_targets: HashSet<String>,
    events: bool,
}

impl FactSink {
    fn new(query: &SessionQuery) -> Self {
        let tools = query.needles(FieldId::Tool);
        let targets = query.needles(FieldId::Touched);
        let flags = query.needles(FieldId::Has);
        Self {
            facts: ReducedFacts {
                models: query
                    .needles(FieldId::Model)
                    .into_iter()
                    .map(|n| (n, ModelMatch::default()))
                    .collect(),
                ..Default::default()
            },
            events: !tools.is_empty()
                || !targets.is_empty()
                || flags.iter().any(|f| matches!(f.as_str(), "errors" | "tools")),
            tools,
            targets,
            pending_targets: HashSet::new(),
        }
    }

    fn note_target(&mut self, target: String) {
        if self.facts.targets.len() == self.targets.len() {
            return;
        }
        // A match inside the target survives a later cwd prefix. Absolute paths are already final;
        // only unmatched relative paths can acquire a new match when the final cwd arrives.
        let lowered = target.to_lowercase();
        for needle in &self.targets {
            if lowered.contains(needle) {
                self.facts.targets.insert(needle.clone());
            }
        }
        if self.facts.targets.len() == self.targets.len() {
            self.pending_targets.clear();
        } else if !std::path::Path::new(&target).is_absolute() {
            self.pending_targets.insert(target);
        }
    }

    fn finish(mut self, session: &Session) -> ReducedFacts {
        for p in self.pending_targets.drain() {
            let path = std::path::Path::new(&p);
            let resolved = match session.cwd.as_deref() {
                Some(cwd) => cwd.join(path).to_string_lossy().to_lowercase(),
                None => p.to_lowercase(),
            };
            for needle in &self.targets {
                if resolved.contains(needle) {
                    self.facts.targets.insert(needle.clone());
                }
            }
        }
        if let Some(model) = session.model.as_deref() {
            self.facts.note_model(model);
        }
        self.facts
    }
}

impl crate::MessageSink for FactSink {
    fn message(&mut self, m: Message) -> crate::Flow {
        if let Some(model) = m.model.as_deref() {
            self.facts.note_model(model);
        }
        self.facts.images |= m.content.iter().any(|b| matches!(b, Block::Image { .. }));
        if self.events {
            for e in events::extract(&m, 0, None) {
                self.facts.errors |= e.kind == "error";
                self.facts.has_tools |= e.tool.is_some();
                if !self.tools.is_empty() {
                    if let Some(tool) = e.tool {
                        let tool = tool.to_lowercase();
                        for needle in &self.tools {
                            if tool.contains(needle) {
                                self.facts.tools.insert(needle.clone());
                            }
                        }
                    }
                }
                if !self.targets.is_empty() && matches!(e.kind, "file_edit" | "file_read") {
                    if let Some(target) = e.target {
                        self.note_target(target);
                    }
                }
            }
        }
        crate::Flow::Continue
    }
}

/// Evaluate a query in one lazy, counted pass, retaining only metadata and distinct predicate
/// facts. The parsed-session convenience [`matches_full`] remains available to callers that
/// already own a transcript. Forest/workflow/compaction walks remain memoized and evaluated on
/// demand; compaction detection keeps its separate extra-enabled pass for validation compatibility.
pub fn matches_streamed(
    query: &SessionQuery,
    adapter: &dyn crate::harness::Adapter,
    r: &SessionRef,
    text_sets: &TextSets,
) -> anyhow::Result<bool> {
    matches_streamed_with_probe(query, adapter, r, text_sets, &mut crate::source::Probe::default())
}

fn matches_streamed_with_probe(
    query: &SessionQuery,
    adapter: &dyn crate::harness::Adapter,
    r: &SessionRef,
    text_sets: &TextSets,
    probe: &mut crate::source::Probe,
) -> anyhow::Result<bool> {
    let mut sink = FactSink::new(query);
    let affordances = query
        .needles(FieldId::Has)
        .iter()
        .any(|flag| crate::affordance::Kind::parse(flag).is_some());
    // Reuse the counted pass's classifier and side-store rows instead of extracting a second
    // set of affordance rows merely to ask which kinds occurred. The operation's probe also
    // shares sibling/directory metadata across refs, both here and on the cached fast path.
    let session = if affordances {
        let source = probe.snapshot(r);
        let (session, counted) = crate::stream::stream_counting(adapter, r, &crate::ParseOptions::lazy(), &mut sink)?;
        if let Some(counted) = counted {
            sink.facts.counts = Some(counted.counts.clone());
            counted.record_with_source(r, &source);
        }
        session
    } else {
        // Preserve the recording path's conditional side-store reads when no affordance result
        // is demanded. Only the cached affordance fast path requires cross-ref probing here.
        crate::stream::stream_counted(adapter, r, &crate::ParseOptions::lazy(), &mut sink)?
    };
    let reduced = sink.finish(&session);
    let facts = SessionFacts {
        reduced: Some(&reduced),
        ..SessionFacts::new(r, &session, text_sets)
    };
    Ok(query.matches(&Facts {
        r,
        session: Some(&session),
        ext: &facts,
    }))
}

/// Facts available before parsing: indexed text membership and fresh catalog affordance counts.
/// Other external predicates stay `Unknown`, preserving the query's three-valued evaluation.
struct RefFacts<'a> {
    id: &'a str,
    text_sets: &'a TextSets,
    counts: Option<crate::affordance::Counts>,
}

impl ExtFacts for RefFacts<'_> {
    fn text(&self, needle: &str) -> Tri {
        Tri::of(self.text_sets.contains(needle, self.id))
    }

    fn has(&self, flag: &str) -> Tri {
        match (crate::affordance::Kind::parse(flag), &self.counts) {
            (Some(k), Some(counts)) => Tri::of(counts.get(&k).is_some_and(|n| *n > 0)),
            _ => Tri::Unknown,
        }
    }
}

/// Apply a parsed query to a ref list in place: prune with the catalog-cheap prefilter, then — if
/// the query has any parse/index/events/forest/counted term — evaluate each survivor against the
/// indexed text membership and catalog's affordance counts, keeping a definite match and dropping
/// a definite miss without a parse, and parse only what is still undecided. `cv ls`/`stats` and
/// `cv serve`'s `/api/stats?q=` all go through here, so `-q` means the same thing on every door.
pub fn filter_refs(refs: &mut Vec<SessionRef>, query: &SessionQuery, text_sets: &TextSets) {
    refs.retain(|r| query.prefilter(r));
    let heavy = query.needs_parse() || query.needs_index() || query.needs_events() || query.needs_forest();
    if !heavy && !query.needs_counted() {
        return;
    }
    let table = query.needs_counted().then(crate::catalog::AffordanceTable::load);
    let mut probe = crate::source::Probe::default();
    refs.retain(|r| {
        let cheap_facts = RefFacts {
            id: &r.id,
            text_sets,
            counts: table
                .as_ref()
                .and_then(|t| t.counts_with_probe(r, &probe.snapshot(r), &mut probe)),
        };
        let cheap = query.eval(&Facts {
            r,
            session: None,
            ext: &cheap_facts,
        });
        match cheap {
            Tri::True => true,
            Tri::False => false,
            // Undecided: reduce it (which also records its counts for the next query).
            Tri::Unknown => crate::harness::for_harness(r.harness).is_some_and(|a| {
                matches_streamed_with_probe(query, a.as_ref(), r, text_sets, &mut probe).unwrap_or(false)
            }),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::Role;

    fn missing_source() -> SessionRef {
        SessionRef {
            id: "indexed-session".into(),
            harness: crate::Harness::Claude,
            path: std::env::temp_dir().join(format!("cv-query-missing-{}.jsonl", uuid::Uuid::new_v4())),
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 0,
        }
    }

    fn text_sets(query: &SessionQuery, r: &SessionRef) -> TextSets {
        TextSets::resolve(query, |needle| {
            if needle == "hit" {
                HashSet::from([r.id.clone()])
            } else {
                HashSet::new()
            }
        })
    }

    #[test]
    fn indexed_text_queries_filter_without_readable_source() {
        let r = missing_source();
        assert!(!r.path.exists());
        for (source, matches) in [
            ("text:hit", true),
            ("text:miss", false),
            ("NOT text:miss", true),
            ("NOT text:hit", false),
            ("text:hit OR tool:bash", true),
            ("tool:bash OR text:hit", true),
            ("text:miss AND tool:bash", false),
            ("tool:bash AND text:miss", false),
            ("NOT (text:miss AND tool:bash)", true),
        ] {
            let query = SessionQuery::parse(source).unwrap();
            let texts = text_sets(&query, &r);
            let mut refs = vec![r.clone()];
            filter_refs(&mut refs, &query, &texts);
            assert_eq!(refs.len(), usize::from(matches), "{source}");
        }
    }

    #[test]
    fn indexed_membership_preserves_unknown_boolean_branches() {
        let r = missing_source();
        for (source, expected) in [
            ("text:miss AND tool:bash", Tri::False),
            ("tool:bash AND text:miss", Tri::False),
            ("text:hit AND tool:bash", Tri::Unknown),
            ("text:hit OR tool:bash", Tri::True),
            ("text:miss OR tool:bash", Tri::Unknown),
            ("NOT (text:hit AND tool:bash)", Tri::Unknown),
            ("NOT (text:miss OR tool:bash)", Tri::Unknown),
            ("NOT text:hit OR tool:bash", Tri::Unknown),
            ("NOT text:miss AND tool:bash", Tri::Unknown),
        ] {
            let query = SessionQuery::parse(source).unwrap();
            let texts = text_sets(&query, &r);
            let ext = RefFacts {
                id: &r.id,
                text_sets: &texts,
                counts: None,
            };
            assert_eq!(
                query.eval(&Facts {
                    r: &r,
                    session: None,
                    ext: &ext
                }),
                expected,
                "{source}"
            );
        }
    }

    #[test]
    fn streamed_facts_match_collection_with_late_metadata_and_boolean_queries() {
        let world = cv_testkit::World::in_process("streamed_query_facts");
        let payload = "rich escaped\n\"🌙\"".repeat(400);
        let path = world.write_session("facts", &[
            serde_json::json!({"type":"assistant","uuid":"early","message":{"role":"assistant", "model":"EarlyModel",
                "content":[{"type":"tool_use","id":"e","name":"Edit","input":{"file_path":"./src/A.rs","rich":payload}},
                    {"type":"tool_use","id":"r","name":"Read","input":{"file_path":"../other.rs"}},
                    {"type":"tool_use","id":"a","name":"Read","input":{"file_path":"/Absolute/Λ.rs"}},
                    {"type":"tool_use","id":"b","name":"Bash","input":{"command":"git status"}}]}}),
            serde_json::json!({"type":"user","uuid":"image","message":{"role":"user","content":[
                {"type":"image","source":{"type":"base64","media_type":"image/png","data":payload}},
                {"type":"tool_result","tool_use_id":"e","is_error":true,"content":"permission denied"}]}}),
            serde_json::json!({"type":"system","uuid":"boundary","subtype":"compact_boundary",
                "compactMetadata":{"trigger":"auto","preTokens":1000}}),
            serde_json::json!({"type":"user","uuid":"summary","parentUuid":"boundary","isCompactSummary":true,
                "message":{"role":"user","content":payload}}),
            serde_json::json!({"type":"assistant","uuid":"late","cwd":"/Late/Work","permissionMode":"plan",
                "gitBranch":"FeatureBranch","message":{"role":"assistant","model":"LateModel","content":[
                    {"type":"tool_use","id":"e2","name":"Edit","input":{"file_path":"./src/A.rs","rich":payload}},
                    {"type":"text","text":"done"}]}}),
            serde_json::json!({"type":"ai-title","aiTitle":"final title"}),
        ]);
        let tasks = world.home.join(".claude/tasks/facts");
        std::fs::create_dir_all(&tasks).unwrap();
        std::fs::write(
            tasks.join("1.json"),
            r#"{"id":"1","subject":"side-store plan","status":"pending"}"#,
        )
        .unwrap();
        let mut r = SessionRef::new("facts", crate::Harness::Claude, path);
        r.cwd = Some("/Stale/Discovery".into());
        r.title = Some("discovery title".into());
        let adapter = crate::harness::for_harness(r.harness).unwrap();
        let full = crate::stream::collect_with(adapter.as_ref(), &r, &crate::ParseOptions::lazy()).unwrap();
        assert_eq!(full.cwd.as_deref(), Some(std::path::Path::new("/Late/Work")));
        for required in [
            "model=earlymodel AND model=latemodel",
            "tool:edit AND tool:bash",
            "touched:/late/work/src/a.rs AND has:images AND has:errors",
            "has:plan",
            "compactions=1",
        ] {
            assert!(
                matches_full(&SessionQuery::parse(required).unwrap(), &r, &full, &TextSets::default()),
                "{required}"
            );
        }
        for source in [
            "model:early",
            "model=earlymodel",
            "model=early",
            "model:late",
            "model:absent",
            "model:early AND model:late",
            "NOT model:early",
            "NOT model:absent",
            "git:feature",
            "git:absent",
            "tool:edit",
            "tool:bash",
            "tool:absent",
            "touched:/late/work/src/a.rs",
            "touched:/late/work/../other.rs",
            "touched:/absolute/λ.rs",
            "touched:/stale/discovery",
            "has:errors",
            "has:tools",
            "has:images",
            "has:compacted",
            "has:plan",
            "has:approval",
            "NOT has:goal",
            "compactions=1",
            "compactions>1",
            "agents=0",
            "workflows=0",
            "NOT has:subagents",
            "NOT has:workflows",
            "text:hit AND tool:edit",
            "text:miss OR model:late",
            "NOT (text:hit AND tool:absent)",
            "(model:absent OR touched:/late/work/src/a.rs) AND NOT has:goal",
            "has:plan AND model:early AND NOT (tool:absent OR has:goal)",
            "title:discovery AND model:late",
            "title:final OR model:absent",
        ] {
            let query = SessionQuery::parse(source).unwrap();
            let texts = TextSets::resolve(&query, |n| {
                if n == "hit" {
                    HashSet::from([r.id.clone()])
                } else {
                    HashSet::new()
                }
            });
            let expected = matches_full(&query, &r, &full, &texts);
            assert_eq!(
                matches_streamed(&query, adapter.as_ref(), &r, &texts).unwrap(),
                expected,
                "{source}"
            );
            let mut refs = vec![r.clone()];
            filter_refs(&mut refs, &query, &texts);
            assert_eq!(!refs.is_empty(), expected, "filter: {source}");
        }
    }

    #[test]
    fn fact_reducer_retains_distinct_facts_without_payloads_or_event_rows() {
        let query = SessionQuery::parse("model:early AND tool:edit AND touched:a.rs AND has:images").unwrap();
        let mut sink = FactSink::new(&query);
        let mut m = Message::new(Role::Assistant);
        m.model = Some("EarlyModel".into());
        m.content.push(Block::ToolUse {
            id: "call".into(),
            name: "Edit".into(),
            namespace: None,
            input: serde_json::json!({"file_path":"./a.rs","rich":"discarded rich argument".repeat(8192)}),
        });
        m.content.push(Block::Image {
            media_type: None,
            data_ref: Some("discarded image reference".repeat(8192)),
        });
        for i in 0..1000 {
            m.model = Some(format!("EarlyModel-{i}"));
            if let Block::ToolUse { name, .. } = &mut m.content[0] {
                *name = format!("Edit-{i}");
            }
            assert_eq!(crate::MessageSink::message(&mut sink, m.clone()), crate::Flow::Continue);
        }
        assert_eq!(sink.facts.models.len(), 1);
        assert_eq!(sink.facts.tools.len(), 1);
        assert_eq!(sink.facts.targets.len(), 1);
        assert!(sink.facts.images);
        assert!(sink.pending_targets.is_empty());
        let mut shell = Session::new("facts", crate::Harness::Claude);
        shell.cwd = Some("/late".into());
        let facts = sink.finish(&shell);
        assert_eq!(facts.targets, HashSet::from(["a.rs".into()]));

        assert!(facts.models["early"].contains);
        assert!(!facts.models["early"].exact);
        assert!(shell.messages.is_empty());
    }

    #[test]
    fn touched_reduction_discards_absolute_paths_and_waits_for_final_cwd() {
        let query = SessionQuery::parse("touched:/late/work/a.rs AND touched:/absolute/wanted.rs").unwrap();
        let mut sink = FactSink::new(&query);
        for i in 0..1000 {
            sink.note_target(format!("/absolute/unmatched-{i}.rs"));
        }
        assert!(sink.pending_targets.is_empty());
        assert!(sink.facts.targets.is_empty());
        sink.note_target("a.rs".into());
        sink.note_target("/absolute/wanted.rs".into());
        assert_eq!(sink.pending_targets, HashSet::from(["a.rs".into()]));
        assert_eq!(sink.facts.targets, HashSet::from(["/absolute/wanted.rs".into()]));
        let mut shell = Session::new("facts", crate::Harness::Claude);
        shell.cwd = Some("/late/work".into());
        assert_eq!(
            sink.finish(&shell).targets,
            HashSet::from(["/late/work/a.rs".into(), "/absolute/wanted.rs".into()])
        );
    }

    #[test]
    fn streamed_compaction_facts_preserve_full_detector_validation() {
        let world = cv_testkit::World::in_process("streamed_compaction_validation");
        let path = world.transcript("invalid-sidecar");
        // The legacy lean reader tolerates overflow inside this ignored sidecar. The extra-enabled
        // compaction detector rejects the record, so its count differs from the lazy IR's kinds.
        std::fs::write(
            &path,
            r#"{"type":"system","subtype":"compact_boundary","toolUseResult":{"ignored":1e400}}"#,
        )
        .unwrap();
        let r = SessionRef::new("invalid-sidecar", crate::Harness::Claude, path);
        let adapter = crate::harness::for_harness(r.harness).unwrap();
        let full = crate::stream::collect_with(adapter.as_ref(), &r, &crate::ParseOptions::lazy()).unwrap();
        let query = SessionQuery::parse("compactions=0").unwrap();
        let texts = TextSets::default();
        assert_eq!(full.messages.len(), 1);
        assert!(matches_full(&query, &r, &full, &texts));
        assert!(matches_streamed(&query, adapter.as_ref(), &r, &texts).unwrap());
    }
}
