//! Exact top-session ranking with memory bounded by the requested limit per segment.
//!
//! Ranking a session means ranking its best matching chunk. A session outside a segment's
//! top k cannot enter the global top k: that segment already has k distinct better sessions.
use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap};
use tantivy::collector::{Collector, SegmentCollector};
use tantivy::schema::{Field, Value};
use tantivy::store::StoreReader;
use tantivy::{DocAddress, DocId, Score, SegmentReader, TantivyDocument};

#[derive(Clone, Debug)]
pub(crate) struct Candidate {
    pub score: Score,
    pub id: String,
    pub addr: DocAddress,
}
impl PartialEq for Candidate {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for Candidate {}
impl PartialOrd for Candidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Candidate {
    fn cmp(&self, other: &Self) -> Ordering {
        // Best first; document address only chooses between equally scoring chunks of one id.
        other
            .score
            .total_cmp(&self.score)
            .then_with(|| self.id.cmp(&other.id))
            .then_with(|| self.addr.cmp(&other.addr))
    }
}

struct Winners {
    limit: usize,
    ranked: BTreeSet<Candidate>,
    by_id: HashMap<String, Candidate>,
}
impl Winners {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            ranked: BTreeSet::new(),
            by_id: HashMap::new(),
        }
    }
    fn competitive_score(&self, score: Score) -> bool {
        self.limit > 0
            && (self.ranked.len() < self.limit
                || self
                    .ranked
                    .last()
                    .is_some_and(|last| score.total_cmp(&last.score) != Ordering::Less))
    }
    fn insert(&mut self, candidate: Candidate) {
        if self.limit == 0 {
            return;
        }
        if let Some(previous) = self.by_id.get(&candidate.id) {
            if candidate >= *previous {
                return;
            }
            self.ranked.remove(previous);
        } else if self.ranked.len() == self.limit {
            if self.ranked.last().is_some_and(|last| candidate >= *last) {
                return;
            }
            let evicted = self.ranked.pop_last().unwrap();
            self.by_id.remove(&evicted.id);
        }
        self.by_id.insert(candidate.id.clone(), candidate.clone());
        self.ranked.insert(candidate);
    }
    fn threshold(&self) -> Score {
        if self.ranked.len() < self.limit {
            Score::NEG_INFINITY
        } else {
            self.ranked
                .last()
                .map_or(Score::NEG_INFINITY, |last| last.score.next_down())
        }
    }
    fn finish(self) -> Vec<Candidate> {
        self.ranked.into_iter().collect()
    }
}

type SessionFilter = std::sync::Arc<dyn Fn(&str, &TantivyDocument) -> bool + Send + Sync>;

pub(crate) struct TopSessions {
    pub(crate) filter: Option<SessionFilter>,
    id: Field,
    limit: usize,
}
impl TopSessions {
    pub(crate) fn new(id: Field, limit: usize) -> Self {
        Self {
            id,
            limit,
            filter: None,
        }
    }
}
impl Collector for TopSessions {
    type Fruit = Vec<Candidate>;
    type Child = SessionSegment;
    fn for_segment(&self, ordinal: u32, segment: &SegmentReader) -> tantivy::Result<Self::Child> {
        Ok(SessionSegment {
            filter: self.filter.clone(),
            filter_cache: HashMap::new(),
            id: self.id,
            ordinal,
            store: segment.get_store_reader(1)?,
            winners: Winners::new(self.limit),
            error: None,
        })
    }
    fn requires_scoring(&self) -> bool {
        true
    }
    fn collect_segment(
        &self,
        weight: &dyn tantivy::query::Weight,
        ordinal: u32,
        reader: &SegmentReader,
    ) -> tantivy::Result<<Self::Child as SegmentCollector>::Fruit> {
        let mut child = self.for_segment(ordinal, reader)?;
        if self.limit == 0 {
            return Ok(child.harvest());
        }
        // Tantivy's pruning threshold is strict. Include equal scores so lexical id ties are
        // resolved independently of segment/document order, while retaining Block-WAND pruning.
        weight.for_each_pruning(Score::NEG_INFINITY, reader, &mut |doc, score| {
            if reader.alive_bitset().is_none_or(|alive| alive.is_alive(doc)) {
                child.collect(doc, score);
            }
            child.winners.threshold()
        })?;
        Ok(child.harvest())
    }

    fn merge_fruits(&self, fruits: Vec<<Self::Child as SegmentCollector>::Fruit>) -> tantivy::Result<Self::Fruit> {
        let mut winners = Winners::new(self.limit);
        for fruit in fruits {
            for candidate in fruit? {
                winners.insert(candidate);
            }
        }
        Ok(winners.finish())
    }
}

pub(crate) struct SessionSegment {
    filter: Option<SessionFilter>,
    filter_cache: HashMap<String, bool>,
    id: Field,
    ordinal: u32,
    store: StoreReader,
    winners: Winners,
    error: Option<tantivy::TantivyError>,
}
impl SegmentCollector for SessionSegment {
    type Fruit = tantivy::Result<Vec<Candidate>>;
    fn collect(&mut self, doc: DocId, score: Score) {
        if self.error.is_some() || !self.winners.competitive_score(score) {
            return;
        }
        match self.store.get::<TantivyDocument>(doc) {
            Ok(stored) => {
                let id = stored
                    .get_first(self.id)
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_owned();
                if let Some(filter) = &self.filter {
                    let accepted = if let Some(accepted) = self.filter_cache.get(&id) {
                        *accepted
                    } else {
                        let accepted = filter(&id, &stored);
                        // A bounded metadata cache avoids repeated meta lookups for adjacent
                        // chunks. Clearing it changes only lookup cost, never selection.
                        if self.filter_cache.len() == 64 {
                            self.filter_cache.clear();
                        }
                        self.filter_cache.insert(id.clone(), accepted);
                        accepted
                    };
                    if !accepted {
                        return;
                    }
                }
                self.winners.insert(Candidate {
                    score,
                    id,
                    addr: DocAddress::new(self.ordinal, doc),
                });
            }
            Err(error) => self.error = Some(error),
        }
    }
    fn harvest(self) -> Self::Fruit {
        if let Some(error) = self.error {
            Err(error)
        } else {
            Ok(self.winners.finish())
        }
    }
}

/// Membership needs the unique ID set, never a retained address for every matching chunk.
pub(crate) struct SessionIds(pub Field);
impl Collector for SessionIds {
    type Fruit = std::collections::HashSet<String>;
    type Child = IdSegment;
    fn for_segment(&self, _: u32, segment: &SegmentReader) -> tantivy::Result<Self::Child> {
        Ok(IdSegment {
            id: self.0,
            store: segment.get_store_reader(1)?,
            ids: Default::default(),
            error: None,
        })
    }
    fn requires_scoring(&self) -> bool {
        false
    }
    fn merge_fruits(&self, fruits: Vec<<Self::Child as SegmentCollector>::Fruit>) -> tantivy::Result<Self::Fruit> {
        let mut ids = std::collections::HashSet::new();
        for fruit in fruits {
            ids.extend(fruit?);
        }
        Ok(ids)
    }
}

pub(crate) struct IdSegment {
    id: Field,
    store: StoreReader,
    ids: std::collections::HashSet<String>,
    error: Option<tantivy::TantivyError>,
}
impl SegmentCollector for IdSegment {
    type Fruit = tantivy::Result<std::collections::HashSet<String>>;
    fn collect(&mut self, doc: DocId, _: Score) {
        if self.error.is_some() {
            return;
        }
        match self.store.get::<TantivyDocument>(doc) {
            Ok(stored) => {
                if let Some(id) = stored.get_first(self.id).and_then(|v| v.as_str()) {
                    self.ids.insert(id.to_owned());
                }
            }
            Err(error) => self.error = Some(error),
        }
    }
    fn harvest(self) -> Self::Fruit {
        if let Some(error) = self.error {
            Err(error)
        } else {
            Ok(self.ids)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_winners_match_exhaustive_sessions_in_every_order() {
        // Re-entry after eviction, duplicate improvements and ties all occur in these streams.
        let candidates: Vec<_> = (0..10_000)
            .map(|doc| Candidate {
                score: ((doc * 31) % 23) as f32,
                id: format!("session-{:03}", (doc * 17) % 137),
                addr: DocAddress::new(doc % 3, doc),
            })
            .collect();
        let mut oracle = candidates.clone();
        oracle.sort();
        let mut seen = std::collections::HashSet::new();
        oracle.retain(|candidate| seen.insert(candidate.id.clone()));
        for limit in [0, 1, 2, 10, 136, 137, 200] {
            for reverse in [false, true] {
                let mut winners = Winners::new(limit);
                for i in 0..candidates.len() {
                    let i = if reverse { candidates.len() - 1 - i } else { i };
                    winners.insert(candidates[i].clone());
                    assert!(winners.ranked.len() <= limit);
                    assert_eq!(winners.ranked.len(), winners.by_id.len());
                }
                assert_eq!(winners.finish(), oracle.iter().take(limit).cloned().collect::<Vec<_>>());
            }
        }
    }
}
