You write search queries for a code-search training set. For each code
symbol you are shown, write what a developer would type into a semantic code
search when they need that code but do not know its name.

Each symbol comes with its file path, its parent (owning type, module or
trait impl), its doc comment as written, and its source span (at most 150
lines). Read the code itself; the doc comment may be missing, short or wrong.

For each symbol write:

- `phrases`: 2 phrases of 3 to 7 words.
- `sentences`: 2 sentences of 10 to 25 words.
- `paraphrases`: 1 paraphrase of each sentence, in the same order: the same
  need in different words.
- `answersAlso`: names of other symbols you believe answer these queries as
  well as this one (an empty list when you know of none). This is a hint for
  a later review, not a label.

Rules:

1. Describe behaviour: what the code does or what problem it solves, as the
   person searching would put it.
2. Do not use the symbol's name, or any word taken from it or from its
   parent's name, its parameters or its file path. Write as someone who
   does not know them would.
3. Do not copy or lightly reword the doc comment's phrasing.
4. Each query must make sense on its own, without the other queries.
5. Write plain English, no code, no backticks, no quotes.

Answer with the JSON object the response schema describes: one entry per
symbol, in the order given, each with the symbol's `n`.

Examples, over a toy snippet that is not part of any data set:

```
n: 1
path: src/shapes/area.rs
parent: shapes::area
doc: Computes the area enclosed by a closed polyline.
code:
pub fn polygon_area(points: &[(f64, f64)]) -> f64 {
    let mut sum = 0.0;
    for i in 0..points.len() {
        let (x1, y1) = points[i];
        let (x2, y2) = points[(i + 1) % points.len()];
        sum += x1 * y2 - x2 * y1;
    }
    (sum / 2.0).abs()
}
```

```fewshot
surface enclosed by a ring of vertices
shoelace formula over a point list
how much flat space does a closed outline of coordinates cover on the plane
given corner coordinates in order, work out the size of the region they bound
size of the region bounded by an ordered list of corners
compute how large the flat region inside a closed chain of coordinates is
```

The answer for this example would be
`{"targets": [{"n": 1, "phrases": [<lines 1-2>], "sentences": [<lines 3-4>],
"paraphrases": [<line 5 as paraphrase of 3>, <line 6 as paraphrase of 4>],
"answersAlso": []}]}`.
