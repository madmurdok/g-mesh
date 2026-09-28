//! The lexical arm (D7, informational): Okapi BM25 over the same node texts
//! the models embed. Terms are the lower-cased camel/snake pieces of every
//! alphanumeric run, so `getEncodingFromHeaders` and "encoding from headers"
//! share terms.

use std::collections::HashMap;

const K1: f64 = 1.2;
const B: f64 = 0.75;

pub fn terms(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for word in text.split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| !w.is_empty()) {
        let chars: Vec<char> = word.chars().collect();
        let mut start = 0;
        for i in 1..=chars.len() {
            let boundary = i == chars.len() || {
                let (prev, cur) = (chars[i - 1], chars[i]);
                let next_lower = chars.get(i + 1).is_some_and(|c| c.is_ascii_lowercase());
                (prev.is_ascii_lowercase() && cur.is_ascii_uppercase())
                    || (prev.is_ascii_uppercase() && cur.is_ascii_uppercase() && next_lower)
                    || (prev.is_ascii_alphabetic() != cur.is_ascii_alphabetic())
            };
            if boundary {
                let piece: String = chars[start..i].iter().collect::<String>().to_ascii_lowercase();
                if piece.len() >= 2 {
                    out.push(piece);
                }
                start = i;
            }
        }
    }
    out
}

pub struct Bm25 {
    doc_terms: Vec<HashMap<String, u32>>,
    doc_len: Vec<f64>,
    avg_len: f64,
    df: HashMap<String, u32>,
}

impl Bm25 {
    pub fn new<'a>(docs: impl IntoIterator<Item = &'a str>) -> Self {
        let mut doc_terms = Vec::new();
        let mut doc_len = Vec::new();
        let mut df: HashMap<String, u32> = HashMap::new();
        for doc in docs {
            let mut tf: HashMap<String, u32> = HashMap::new();
            let t = terms(doc);
            doc_len.push(t.len() as f64);
            for term in t {
                *tf.entry(term).or_default() += 1;
            }
            for term in tf.keys() {
                *df.entry(term.clone()).or_default() += 1;
            }
            doc_terms.push(tf);
        }
        let avg_len =
            if doc_len.is_empty() { 0.0 } else { doc_len.iter().sum::<f64>() / doc_len.len() as f64 };
        Self { doc_terms, doc_len, avg_len, df }
    }

    /// One score per document, in construction order.
    pub fn scores(&self, query: &str) -> Vec<f64> {
        let n = self.doc_terms.len() as f64;
        let mut query_terms = terms(query);
        query_terms.sort();
        query_terms.dedup();
        let idf: Vec<(String, f64)> = query_terms
            .into_iter()
            .filter_map(|t| {
                let df = f64::from(*self.df.get(&t)?);
                Some((t, ((n - df + 0.5) / (df + 0.5) + 1.0).ln()))
            })
            .collect();
        self.doc_terms
            .iter()
            .zip(&self.doc_len)
            .map(|(tf, &len)| {
                let norm = K1 * (1.0 - B + B * len / self.avg_len.max(f64::MIN_POSITIVE));
                idf.iter()
                    .map(|(t, w)| {
                        let f = f64::from(*tf.get(t).unwrap_or(&0));
                        w * f * (K1 + 1.0) / (f + norm)
                    })
                    .sum()
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terms_split_identifiers_like_the_overlap_rule() {
        assert_eq!(
            terms("fn getEncodingFromHeaders(h: &HeaderMap)"),
            vec!["fn", "get", "encoding", "from", "headers", "header", "map"]
        );
    }

    /// Control: dropping the idf weight (every term weighs 1) lets twelve
    /// repetitions of the common "the" win, and document 0 comes first.
    #[test]
    fn a_rare_matching_term_outranks_a_common_one() {
        let docs = [
            "the the the the the the the the the the the the",
            "charset",
            "the value of the thing",
            "the other thing",
        ];
        let bm25 = Bm25::new(docs.iter().copied());
        let scores = bm25.scores("the charset");
        let best = scores.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).unwrap().0;
        assert_eq!(best, 1, "{scores:?}");
    }

    #[test]
    fn a_query_with_no_known_term_scores_zero_everywhere() {
        let bm25 = Bm25::new(["alpha beta", "gamma"].iter().copied());
        assert!(bm25.scores("zeta").iter().all(|&s| s == 0.0));
    }
}
