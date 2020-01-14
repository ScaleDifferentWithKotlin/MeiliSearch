use std::borrow::Cow;
use std::collections::HashSet;
use std::convert::TryFrom;
use std::mem;
use std::ops::Deref;
use std::ops::Range;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use std::{cmp, fmt};

use compact_arena::{SmallArena, Idx32, mk_arena};
use fst::{IntoStreamer, Streamer};
use levenshtein_automata::DFA;
use log::debug;
use meilisearch_tokenizer::{is_cjk, split_query_string};
use meilisearch_types::DocIndex;
use sdset::{Set, SetBuf};
use slice_group_by::{GroupBy, GroupByMut};

use crate::automaton::NGRAMS;
use crate::automaton::{build_dfa, build_prefix_dfa, build_exact_dfa};
use crate::automaton::normalize_str;
use crate::automaton::{QueryEnhancer, QueryEnhancerBuilder};

use crate::criterion::{Criteria, Context, ContextMut};
use crate::distinct_map::{BufferedDistinctMap, DistinctMap};
use crate::raw_document::RawDocument;
use crate::{database::MainT, reordered_attrs::ReorderedAttrs};
use crate::{store, Document, DocumentId, MResult};
use crate::query_tree::{create_query_tree, traverse_query_tree, QueryResult};
use crate::query_tree::Context as QTContext;
use crate::store::Postings;

pub fn bucket_sort<'c, FI>(
    reader: &heed::RoTxn<MainT>,
    query: &str,
    range: Range<usize>,
    filter: Option<FI>,
    criteria: Criteria<'c>,
    searchable_attrs: Option<ReorderedAttrs>,
    main_store: store::Main,
    postings_lists_store: store::PostingsLists,
    documents_fields_counts_store: store::DocumentsFieldsCounts,
    synonyms_store: store::Synonyms,
    prefix_documents_cache_store: store::PrefixDocumentsCache,
    prefix_postings_lists_cache_store: store::PrefixPostingsListsCache,
) -> MResult<Vec<Document>>
where
    FI: Fn(DocumentId) -> bool,
{
    // We delegate the filter work to the distinct query builder,
    // specifying a distinct rule that has no effect.
    if filter.is_some() {
        let distinct = |_| None;
        let distinct_size = 1;
        return bucket_sort_with_distinct(
            reader,
            query,
            range,
            filter,
            distinct,
            distinct_size,
            criteria,
            searchable_attrs,
            main_store,
            postings_lists_store,
            documents_fields_counts_store,
            synonyms_store,
            prefix_documents_cache_store,
            prefix_postings_lists_cache_store,
        );
    }

    let words_set = match unsafe { main_store.static_words_fst(reader)? } {
        Some(words) => words,
        None => return Ok(Vec::new()),
    };

    let context = QTContext {
        words_set,
        synonyms: synonyms_store,
        postings_lists: postings_lists_store,
        prefix_postings_lists: prefix_postings_lists_cache_store,
    };

    let (operation, mapping) = create_query_tree(reader, &context, query).unwrap();
    println!("{:?}", operation);
    println!("{:?}", mapping);

    let QueryResult { docids, queries } = traverse_query_tree(reader, &context, &operation).unwrap();
    println!("found {} documents", docids.len());
    println!("number of postings {:?}", queries.len());

    let before = Instant::now();

    let mut bare_matches = Vec::new();
    mk_arena!(arena);
    for ((query, input, distance), matches) in queries {

        let postings_list_view = PostingsListView::original(Rc::from(input), Rc::new(matches));
        // TODO optimize the filter by skipping docids that have already been seen
        let mut offset = 0;
        for matches in postings_list_view.binary_group_by_key(|m| m.document_id) {
            let document_id = matches[0].document_id;
            if docids.contains(&document_id) {
                let range = postings_list_view.range(offset, matches.len());
                let posting_list_index = arena.add(range);
                let bare_match = BareMatch {
                    document_id,
                    query_index: query.id,
                    distance: distance,
                    is_exact: true, // TODO where can I find this info?
                    postings_list: posting_list_index,
                };

                bare_matches.push(bare_match);
            }

            offset += matches.len();
        }
    }

    println!("matches cleaned in {:.02?}", before.elapsed());

    let before_bucket_sort = Instant::now();

    let before_raw_documents_presort = Instant::now();
    bare_matches.sort_unstable_by_key(|sm| sm.document_id);
    debug!("sort by documents ids took {:.02?}", before_raw_documents_presort.elapsed());

    let before_raw_documents_building = Instant::now();
    let mut prefiltered_documents = 0;
    let mut raw_documents = Vec::new();
    for bare_matches in bare_matches.linear_group_by_key_mut(|sm| sm.document_id) {
        let raw_document = RawDocument::new(bare_matches, &mut arena, searchable_attrs.as_ref());
        raw_documents.push(raw_document);
    }
    debug!("creating {} candidates documents took {:.02?}",
        raw_documents.len(),
        before_raw_documents_building.elapsed(),
    );

    let before_criterion_loop = Instant::now();
    let proximity_count = AtomicUsize::new(0);

    let mut groups = vec![raw_documents.as_mut_slice()];

    'criteria: for criterion in criteria.as_ref() {
        let tmp_groups = mem::replace(&mut groups, Vec::new());
        let mut documents_seen = 0;

        for mut group in tmp_groups {
            let before_criterion_preparation = Instant::now();

            let ctx = ContextMut {
                reader,
                postings_lists: &mut arena,
                query_mapping: &mapping,
                documents_fields_counts_store,
            };

            criterion.prepare(ctx, &mut group)?;
            debug!("{:?} preparation took {:.02?}", criterion.name(), before_criterion_preparation.elapsed());

            let ctx = Context {
                postings_lists: &arena,
                query_mapping: &mapping,
            };

            let must_count = criterion.name() == "proximity";

            let before_criterion_sort = Instant::now();
            group.sort_unstable_by(|a, b| {
                if must_count {
                    proximity_count.fetch_add(1, Ordering::SeqCst);
                }

                criterion.evaluate(&ctx, a, b)
            });
            debug!("{:?} evaluation took {:.02?}", criterion.name(), before_criterion_sort.elapsed());

            for group in group.binary_group_by_mut(|a, b| criterion.eq(&ctx, a, b)) {
                debug!("{:?} produced a group of size {}", criterion.name(), group.len());

                documents_seen += group.len();
                groups.push(group);

                // we have sort enough documents if the last document sorted is after
                // the end of the requested range, we can continue to the next criterion
                if documents_seen >= range.end {
                    continue 'criteria;
                }
            }
        }
    }

    debug!("criterion loop took {:.02?}", before_criterion_loop.elapsed());
    debug!("proximity evaluation called {} times", proximity_count.load(Ordering::Relaxed));

    let iter = raw_documents.into_iter().skip(range.start).take(range.len());
    let iter = iter.map(|rd| Document::from_raw(rd, &arena, searchable_attrs.as_ref()));
    let documents = iter.collect();

    debug!("bucket sort took {:.02?}", before_bucket_sort.elapsed());

    Ok(documents)
}

pub fn bucket_sort_with_distinct<'c, FI, FD>(
    reader: &heed::RoTxn<MainT>,
    query: &str,
    range: Range<usize>,
    filter: Option<FI>,
    distinct: FD,
    distinct_size: usize,
    criteria: Criteria<'c>,
    searchable_attrs: Option<ReorderedAttrs>,
    main_store: store::Main,
    postings_lists_store: store::PostingsLists,
    documents_fields_counts_store: store::DocumentsFieldsCounts,
    synonyms_store: store::Synonyms,
    prefix_documents_cache_store: store::PrefixDocumentsCache,
    prefix_postings_lists_cache_store: store::PrefixPostingsListsCache,
) -> MResult<Vec<Document>>
where
    FI: Fn(DocumentId) -> bool,
    FD: Fn(DocumentId) -> Option<u64>,
{
    unimplemented!()
}

pub struct BareMatch<'tag> {
    pub document_id: DocumentId,
    pub query_index: usize,
    pub distance: u8,
    pub is_exact: bool,
    pub postings_list: Idx32<'tag>,
}

impl fmt::Debug for BareMatch<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BareMatch")
            .field("document_id", &self.document_id)
            .field("query_index", &self.query_index)
            .field("distance", &self.distance)
            .field("is_exact", &self.is_exact)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SimpleMatch {
    pub query_index: usize,
    pub distance: u8,
    pub attribute: u16,
    pub word_index: u16,
    pub is_exact: bool,
}

#[derive(Clone)]
pub enum PostingsListView<'txn> {
    Original {
        input: Rc<[u8]>,
        postings_list: Rc<Cow<'txn, Set<DocIndex>>>,
        offset: usize,
        len: usize,
    },
    Rewritten {
        input: Rc<[u8]>,
        postings_list: SetBuf<DocIndex>,
    },
}

impl fmt::Debug for PostingsListView<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PostingsListView")
            .field("input", &std::str::from_utf8(&self.input()).unwrap())
            .field("postings_list", &self.as_ref())
            .finish()
    }
}

impl<'txn> PostingsListView<'txn> {
    pub fn original(input: Rc<[u8]>, postings_list: Rc<Cow<'txn, Set<DocIndex>>>) -> PostingsListView<'txn> {
        let len = postings_list.len();
        PostingsListView::Original { input, postings_list, offset: 0, len }
    }

    pub fn rewritten(input: Rc<[u8]>, postings_list: SetBuf<DocIndex>) -> PostingsListView<'txn> {
        PostingsListView::Rewritten { input, postings_list }
    }

    pub fn rewrite_with(&mut self, postings_list: SetBuf<DocIndex>) {
        let input = match self {
            PostingsListView::Original { input, .. } => input.clone(),
            PostingsListView::Rewritten { input, .. } => input.clone(),
        };
        *self = PostingsListView::rewritten(input, postings_list);
    }

    pub fn len(&self) -> usize {
        match self {
            PostingsListView::Original { len, .. } => *len,
            PostingsListView::Rewritten { postings_list, .. } => postings_list.len(),
        }
    }

    pub fn input(&self) -> &[u8] {
        match self {
            PostingsListView::Original { ref input, .. } => input,
            PostingsListView::Rewritten { ref input, .. } => input,
        }
    }

    pub fn range(&self, range_offset: usize, range_len: usize) -> PostingsListView<'txn> {
        match self {
            PostingsListView::Original { input, postings_list, offset, len } => {
                assert!(range_offset + range_len <= *len);
                PostingsListView::Original {
                    input: input.clone(),
                    postings_list: postings_list.clone(),
                    offset: offset + range_offset,
                    len: range_len,
                }
            },
            PostingsListView::Rewritten { .. } => {
                panic!("Cannot create a range on a rewritten postings list view");
            }
        }
    }
}

impl AsRef<Set<DocIndex>> for PostingsListView<'_> {
    fn as_ref(&self) -> &Set<DocIndex> {
        self
    }
}

impl Deref for PostingsListView<'_> {
    type Target = Set<DocIndex>;

    fn deref(&self) -> &Set<DocIndex> {
        match *self {
            PostingsListView::Original { ref postings_list, offset, len, .. } => {
                Set::new_unchecked(&postings_list[offset..offset + len])
            },
            PostingsListView::Rewritten { ref postings_list, .. } => postings_list,
        }
    }
}

fn fetch_matches<'txn, 'tag>(
    reader: &'txn heed::RoTxn<MainT>,
    automatons: &[QueryWordAutomaton],
    arena: &mut SmallArena<'tag, PostingsListView<'txn>>,
    main_store: store::Main,
    postings_lists_store: store::PostingsLists,
    pplc_store: store::PrefixPostingsListsCache,
) -> MResult<Vec<BareMatch<'tag>>>
{
    let before_words_fst = Instant::now();
    let words = match unsafe { main_store.static_words_fst(reader)? } {
        Some(words) => words,
        None => return Ok(Vec::new()),
    };
    debug!("words fst took {:.02?}", before_words_fst.elapsed());
    debug!("words fst len {} and size {}", words.len(), words.as_fst().as_bytes().len());

    let mut total_postings_lists = Vec::new();
    let mut documents_ids = HashSet::<DocumentId>::new();

    let mut dfa_time = Duration::default();
    let mut postings_lists_fetching_time = Duration::default();
    let automatons_loop = Instant::now();

    for (query_index, automaton) in automatons.iter().enumerate() {
        let QueryWordAutomaton { query, is_exact, is_prefix, .. } = automaton;

        let before_word_postings_lists_fetching = Instant::now();
        let mut stream_next_time = Duration::default();
        let mut number_of_words = 0;
        let mut postings_lists_original_length = 0;
        let mut postings_lists_length = 0;

        if *is_prefix && query.len() == 1 {
            let prefix = [query.as_bytes()[0], 0, 0, 0];

            number_of_words += 1;

            let before_postings_lists_fetching = Instant::now();
            if let Some(postings) = pplc_store.prefix_postings_list(reader, prefix)? {
                debug!("Found cached postings list for {:?}", query);
                postings_lists_original_length += postings.matches.len();

                let input = Rc::from(&prefix[..]);
                let postings_list = Rc::new(postings.matches);
                let postings_list_view = PostingsListView::original(input, postings_list);

                let mut offset = 0;
                for group in postings_list_view.linear_group_by_key(|di| di.document_id) {
                    let document_id = group[0].document_id;

                    if query_index != 0 && !documents_ids.contains(&document_id) {
                        offset += group.len();
                        continue
                    }
                    documents_ids.insert(document_id);

                    postings_lists_length += group.len();

                    let range = postings_list_view.range(offset, group.len());
                    let posting_list_index = arena.add(range);
                    let bare_match = BareMatch {
                        document_id,
                        query_index,
                        distance: 0,
                        is_exact: *is_exact,
                        postings_list: posting_list_index,
                    };


                    total_postings_lists.push(bare_match);
                    offset += group.len();
                }
            }
            postings_lists_fetching_time += before_postings_lists_fetching.elapsed();
        }
        else {
            let before_dfa = Instant::now();
            let dfa = automaton.dfa();
            dfa_time += before_dfa.elapsed();

            let byte = query.as_bytes()[0];
            let mut stream = if byte == u8::max_value() {
                words.search(&dfa).ge(&[byte]).into_stream()
            } else {
                words.search(&dfa).ge(&[byte]).lt(&[byte + 1]).into_stream()
            };

            // while let Some(input) = stream.next() {
            loop {
                let before_stream_next = Instant::now();
                let value = stream.next();
                stream_next_time += before_stream_next.elapsed();

                let input = match value {
                    Some(input) => input,
                    None => break,
                };

                number_of_words += 1;

                let distance = dfa.eval(input).to_u8();
                let is_exact = *is_exact && distance == 0 && input.len() == query.len();

                let before_postings_lists_fetching = Instant::now();
                if let Some(Postings { docids, matches }) = postings_lists_store.postings_list(reader, input)? {
                    postings_lists_original_length += matches.len();

                    let input = Rc::from(input);
                    let matches = Rc::new(matches);
                    let postings_list_view = PostingsListView::original(input, matches);

                    let mut offset = 0;
                    for group in postings_list_view.linear_group_by_key(|di| di.document_id) {
                        let document_id = group[0].document_id;

                        if query_index != 0 && !documents_ids.contains(&document_id) {
                            offset += group.len();
                            continue
                        }
                        documents_ids.insert(document_id);

                        postings_lists_length += group.len();

                        let range = postings_list_view.range(offset, group.len());
                        let posting_list_index = arena.add(range);
                        let bare_match = BareMatch {
                            document_id,
                            query_index,
                            distance,
                            is_exact,
                            postings_list: posting_list_index,
                        };

                        total_postings_lists.push(bare_match);
                        offset += group.len();
                    }
                }
                postings_lists_fetching_time += before_postings_lists_fetching.elapsed();
            }
        }

        debug!("{:?} gives {} words", query, number_of_words);
        debug!("{:?} gives postings lists of length {} (original was {})",
            query, postings_lists_length, postings_lists_original_length);
        debug!("{:?} took {:.02?} to fetch postings lists",
            query, before_word_postings_lists_fetching.elapsed());
        debug!("stream next took {:.02?}", stream_next_time);
    }

    debug!("automatons loop took {:.02?}", automatons_loop.elapsed());
    debug!("postings lists fetching took {:.02?}", postings_lists_fetching_time);
    debug!("dfa creation took {:.02?}", dfa_time);

    Ok(total_postings_lists)
}

#[derive(Debug)]
pub struct QueryWordAutomaton {
    pub query: String,
    /// Is it a word that must be considered exact
    /// or is it some derived word (i.e. a synonym)
    pub is_exact: bool,
    pub is_prefix: bool,
    /// If it's a phrase query and what is
    /// its index an the length of the phrase
    pub phrase_query: Option<(u16, u16)>,
}

impl QueryWordAutomaton {
    pub fn exact(query: &str) -> QueryWordAutomaton {
        QueryWordAutomaton {
            query: query.to_string(),
            is_exact: true,
            is_prefix: false,
            phrase_query: None,
        }
    }

    pub fn exact_prefix(query: &str) -> QueryWordAutomaton {
        QueryWordAutomaton {
            query: query.to_string(),
            is_exact: true,
            is_prefix: true,
            phrase_query: None,
        }
    }

    pub fn non_exact(query: &str) -> QueryWordAutomaton {
        QueryWordAutomaton {
            query: query.to_string(),
            is_exact: false,
            is_prefix: false,
            phrase_query: None,
        }
    }

    pub fn dfa(&self) -> DFA {
        if self.phrase_query.is_some() {
            build_exact_dfa(&self.query)
        } else if self.is_prefix {
            build_prefix_dfa(&self.query)
        } else {
            build_dfa(&self.query)
        }
    }
}

fn split_best_frequency<'a>(
    reader: &heed::RoTxn<MainT>,
    word: &'a str,
    postings_lists_store: store::PostingsLists,
) -> MResult<Option<(&'a str, &'a str)>> {
    let chars = word.char_indices().skip(1);
    let mut best = None;

    for (i, _) in chars {
        let (left, right) = word.split_at(i);

        let left_freq = postings_lists_store
            .postings_list(reader, left.as_ref())?
            .map_or(0, |p| p.docids.len());

        let right_freq = postings_lists_store
            .postings_list(reader, right.as_ref())?
            .map_or(0, |p| p.docids.len());

        let min_freq = cmp::min(left_freq, right_freq);
        if min_freq != 0 && best.map_or(true, |(old, _, _)| min_freq > old) {
            best = Some((min_freq, left, right));
        }
    }

    Ok(best.map(|(_, l, r)| (l, r)))
}

fn construct_automatons(
    reader: &heed::RoTxn<MainT>,
    query: &str,
    main_store: store::Main,
    postings_lists_store: store::PostingsLists,
    synonym_store: store::Synonyms,
) -> MResult<(Vec<QueryWordAutomaton>, QueryEnhancer)> {
    let has_end_whitespace = query.chars().last().map_or(false, char::is_whitespace);
    let query_words: Vec<_> = split_query_string(query).map(str::to_lowercase).collect();
    let synonyms = match main_store.synonyms_fst(reader)? {
        Some(synonym) => synonym,
        None => fst::Set::default(),
    };

    let mut automaton_index = 0;
    let mut automatons = Vec::new();
    let mut enhancer_builder = QueryEnhancerBuilder::new(&query_words);

    // We must not declare the original words to the query enhancer
    // *but* we need to push them in the automatons list first
    let mut original_words = query_words.iter().peekable();
    while let Some(word) = original_words.next() {
        let has_following_word = original_words.peek().is_some();
        let not_prefix_dfa = has_following_word || has_end_whitespace || word.chars().all(is_cjk);

        let automaton = if not_prefix_dfa {
            QueryWordAutomaton::exact(word)
        } else {
            QueryWordAutomaton::exact_prefix(word)
        };
        automaton_index += 1;
        automatons.push(automaton);
    }

    for n in 1..=NGRAMS {
        let mut ngrams = query_words.windows(n).enumerate().peekable();
        while let Some((query_index, ngram_slice)) = ngrams.next() {
            let query_range = query_index..query_index + n;
            let ngram_nb_words = ngram_slice.len();
            let ngram = ngram_slice.join(" ");

            let has_following_word = ngrams.peek().is_some();
            let not_prefix_dfa =
                has_following_word || has_end_whitespace || ngram.chars().all(is_cjk);

            // automaton of synonyms of the ngrams
            let normalized = normalize_str(&ngram);
            let lev = if not_prefix_dfa {
                build_dfa(&normalized)
            } else {
                build_prefix_dfa(&normalized)
            };

            let mut stream = synonyms.search(&lev).into_stream();
            while let Some(base) = stream.next() {
                // only trigger alternatives when the last word has been typed
                // i.e. "new " do not but "new yo" triggers alternatives to "new york"
                let base = std::str::from_utf8(base).unwrap();
                let base_nb_words = split_query_string(base).count();
                if ngram_nb_words != base_nb_words {
                    continue;
                }

                if let Some(synonyms) = synonym_store.synonyms(reader, base.as_bytes())? {
                    let mut stream = synonyms.into_stream();
                    while let Some(synonyms) = stream.next() {
                        let synonyms = std::str::from_utf8(synonyms).unwrap();
                        let synonyms_words: Vec<_> = split_query_string(synonyms).collect();
                        let nb_synonym_words = synonyms_words.len();

                        let real_query_index = automaton_index;
                        enhancer_builder.declare(query_range.clone(), real_query_index, &synonyms_words);

                        for synonym in synonyms_words {
                            let automaton = if nb_synonym_words == 1 {
                                QueryWordAutomaton::exact(synonym)
                            } else {
                                QueryWordAutomaton::non_exact(synonym)
                            };
                            automaton_index += 1;
                            automatons.push(automaton);
                        }
                    }
                }
            }

            if n == 1 {
                // automatons for splitted words
                if let Some((left, right)) = split_best_frequency(reader, &normalized, postings_lists_store)? {
                    let mut left_automaton = QueryWordAutomaton::exact(left);
                    left_automaton.phrase_query = Some((0, 2));
                    enhancer_builder.declare(query_range.clone(), automaton_index, &[left]);
                    automaton_index += 1;
                    automatons.push(left_automaton);

                    let mut right_automaton = QueryWordAutomaton::exact(right);
                    right_automaton.phrase_query = Some((1, 2));
                    enhancer_builder.declare(query_range.clone(), automaton_index, &[right]);
                    automaton_index += 1;
                    automatons.push(right_automaton);
                }
            } else {
                // automaton of concatenation of query words
                let concat = ngram_slice.concat();
                let normalized = normalize_str(&concat);

                let real_query_index = automaton_index;
                enhancer_builder.declare(query_range.clone(), real_query_index, &[&normalized]);

                let automaton = QueryWordAutomaton::exact(&normalized);
                automaton_index += 1;
                automatons.push(automaton);
            }
        }
    }

    Ok((automatons, enhancer_builder.build()))
}
