# GM-473 S1: store-lock hold times per tool handler

How long each MCP tool handler holds the index store (`IndexStore::read()`,
a blocking `Mutex<Connection>`) on a tokio worker, and how long a concurrent
call on another session waits for the lock or for a worker. Feeds the GM-473
decision: move every store read off the async workers, or leave them.

## Setup

- **Build:** release, commit 0d25409 (`release-3.18.0`), all workspace
  binaries, plus the lock-log instrumentation in the appendix. The
  instrumentation lives only in a throwaway `git worktree`
  (`g-mesh-wt-gm473-instr`, removed after the run); nothing in product paths
  is committed.
- **Instrumentation:** with `G_MESH_LOCKLOG` set, every store hold appends
  its acquire time, lock wait and hold time; `lock`/`acquire`/`read`/`with`
  are `#[track_caller]`, so each hold is attributed to the `store.read()`
  line of the handler that took it. `find_definition`'s `embed_query` is
  timed separately. The guard is released before the log line is written.
- **Workers:** the daemon runtime is `new_multi_thread().worker_threads(2)`
  (`core/src/daemon/mod.rs:502`, and the front at `core/src/daemon/front.rs:114`).
- **Path:** `g-mesh mcp-shim` over stdio (one shim = one session) against the
  per-project daemon, isolated `G_MESH_HOME`, on two eval corpora:
  g-mesh at 805686f (Rust, 333 files) and excalidraw at 1acf66e (TypeScript,
  629 files, the largest corpus in `eval/embedding/work/corpora`).
- **Queries:** 40 source files sampled (seed 473), outlined; symbol names
  from those outlines drive `find_definition`/`find_references` (any name),
  `find_callers`/`find_callees` (functions), `find_implementations` (types),
  `get_dependencies` alternates Incoming/Outgoing. The semantic rung is driven
  by camel-cased eval phrases (`eval/embedding/queries/<corpus>.jsonl`) that
  match no symbol, so `find_definition` falls to `by_semantic_neighbours`
  (23/23 and 30/30 non-error answers were `resolvedBy: semanticNeighbours`).
  40 calls per arm.
- **Arms per corpus:** index; restart the daemon (model not loaded); *cold*:
  the daemon's first semantic `find_definition` while probes run; *solo*:
  one handler alone, 40 calls; *baseline*: probes alone; *contention*: A loops
  one handler while probe B (`get_file_outline` of a small file, another
  session, every 20 ms) and probe C (`tools/list`, a third session: no store
  lock, so it only waits for a worker) run. Wait = probe latency minus its
  baseline p50.
- **Machine:** every arm passed the gate (1-min load < 4 held 30 s); load
  1.8-3.9 during arms, coming down from ~66 at the dry run. No writer
  (indexing, embedding, semantic pass) took the store during any arm.
  `uptime` and `/usr/bin/time -p` per corpus are in the tables.

## Results

## g-mesh ({'files': 40, 'names': 945, 'fn': 677, 'types': 70, 'misses': 32, 'kinds': {'Function': 753, 'Type': 79, 'Module': 39, 'Variable': 270}}, probe scripts/release-smoke-fixture/rust/src/lib.rs)
uptime start: 14:37  up 2 days,  2:33, 7 users, load averages: 2.19 28.70 43.58  |  end: 14:53  up 2 days,  2:49, 7 users, load averages: 3.17 4.67 17.23
time -p: real 932.86 user 2.77 sys 1.44  |  daemon cpu over arms: 26.4 s  |  index {'ready_s': 3.0, 'quiet_wait_s': 30.1, 'quiet': 'quiet'}  restart {'ready_s': 3.0, 'quiet_wait_s': 30.1, 'quiet': 'quiet'}  |  left alive: []

cold semantic find_definition (open, load 3.06): A 638.3 ms, hold 636.3 ms, first embed 568.2 ms (embeds before 0), semantic=True; B max overlapping 626.646 ms, C (tools/list) max overlapping 638.836 ms
baseline (open): B outline p50/p95 2.43/4.50 ms, C tools/list p50/p95 1.11/2.64 ms

| handler | n | hold p50 | hold p95 | hold max | +req holds p95 | latency p50 | B wait p50 | B wait p95 | B wait max | C wait p95 | C max | gate (solo/cont) | load (solo/cont) | writers |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| get_file_outline | 40 | 0.37 | 0.79 | 1.28 | 1.09 | 1.23 | 0.26 | 1.90 | 2.24 | 3.49 | 6.77 | open/open | 2.82/2.98 | 0/0 |
| get_dependencies | 40 | 1.54 | 4.77 | 13.9 | 5.16 | 2.34 | 0.84 | 3.36 | 4.88 | 3.04 | 6.88 | open/open | 2.35/2.38 | 0/0 |
| find_definition | 40 | 4.86 | 9.17 | 9.25 | 9.44 | 5.78 | 2.20 | 42.6 | 44.5 | 23.4 | 46.0 | open/open | 2.33/3.18 | 0/0 |
| find_references | 40 | 5.45 | 12.6 | 14.7 | 13.1 | 6.63 | 1.59 | 13.7 | 17.4 | 4.73 | 13.7 | open/open | 3.31/3.36 | 0/0 |
| find_callers | 40 | 4.32 | 9.92 | 10.2 | 10.3 | 5.27 | 1.00 | 12.1 | 28.5 | 12.4 | 16.5 | open/open | 3.36/3.19 | 0/0 |
| find_callees | 40 | 4.21 | 8.13 | 8.99 | 8.61 | 5.30 | -0.65 | 8.58 | 11.0 | 6.80 | 9.37 | open/open | 2.89/3.49 | 0/0 |
| find_implementations | 40 | 4.99 | 10.9 | 21.4 | 11.2 | 5.85 | 1.70 | 11.3 | 28.8 | 8.39 | 19.6 | open/open | 3.51/3.51 | 0/0 |
| find_definition_semantic (23/23 sem, embed p50 7.28) | 40 | 62.1 | 68.8 | 70.2 | 69.4 | 63.2 | 97.6 | 105.7 | 117.4 | 43.3 | 65.1 | open/open | 3.05/3.17 | 0/0 |

## excalidraw ({'files': 37, 'names': 298, 'fn': 117, 'types': 29, 'misses': 31, 'kinds': {'Function': 122, 'Variable': 156, 'Type': 29}}, probe packages/element/src/showSelectedShapeActions.ts)
uptime start: 14:53  up 2 days,  2:49, 7 users, load averages: 3.16 4.64 17.15  |  end: 15:07  up 2 days,  3:03, 7 users, load averages: 1.78 2.91 8.40
time -p: real 846.10 user 2.64 sys 1.35  |  daemon cpu over arms: 17.3 s  |  index {'ready_s': 113.6, 'quiet_wait_s': 30.1, 'quiet': 'quiet'}  restart {'ready_s': 3.0, 'quiet_wait_s': 30.1, 'quiet': 'quiet'}  |  left alive: []

cold semantic find_definition (open, load 3.85): A 615.8 ms, hold 614.1 ms, first embed 570.0 ms (embeds before 0), semantic=True; B max overlapping 616.16 ms, C (tools/list) max overlapping 615.788 ms
baseline (open): B outline p50/p95 2.32/4.13 ms, C tools/list p50/p95 1.11/2.92 ms

| handler | n | hold p50 | hold p95 | hold max | +req holds p95 | latency p50 | B wait p50 | B wait p95 | B wait max | C wait p95 | C max | gate (solo/cont) | load (solo/cont) | writers |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| get_file_outline | 40 | 0.22 | 0.35 | 0.86 | 0.74 | 1.11 | -0.82 | -0.03 | 0.28 | 2.39 | 6.89 | open/open | 3.37/3.26 | 0/0 |
| get_dependencies | 40 | 1.01 | 4.68 | 7.01 | 5.02 | 1.85 | -0.39 | 10.8 | 19.4 | 5.14 | 7.52 | open/open | 3.54/3.24 | 0/0 |
| find_definition | 40 | 2.86 | 4.88 | 6.66 | 5.27 | 3.88 | -0.57 | 15.3 | 43.7 | 7.27 | 24.9 | open/open | 3.09/3.16 | 0/0 |
| find_references | 40 | 2.86 | 5.23 | 6.30 | 5.60 | 3.80 | 5.64 | 21.2 | 36.3 | 6.97 | 8.72 | open/open | 2.78/2.92 | 0/0 |
| find_callers | 40 | 2.84 | 5.00 | 6.07 | 5.39 | 3.76 | -0.46 | 5.43 | 6.04 | 4.40 | 9.93 | open/open | 2.16/2.4 | 0/0 |
| find_callees | 40 | 3.01 | 5.68 | 7.00 | 6.00 | 4.00 | 1.41 | 6.25 | 7.25 | 2.53 | 7.12 | open/open | 2.66/2.27 | 0/0 |
| find_implementations | 40 | 2.82 | 3.85 | 5.42 | 4.35 | 3.66 | 0.76 | 2.91 | 5.43 | 4.65 | 8.08 | open/open | 2.84/2.0 | 0/0 |
| find_definition_semantic (30/30 sem, embed p50 7.47) | 40 | 35.8 | 43.1 | 43.6 | 43.4 | 36.8 | 40.2 | 59.7 | 71.2 | 20.0 | 36.4 | open/open | 2.98/1.78 | 0/0 |

All times ms. hold = the handler's own store.read() guard per call, A alone (solo arm); +req holds adds mark_used and ensure_fresh. Wait columns are the contention arm (A loops the handler): B = get_file_outline probe on another session, C = tools/list on a third session (takes no store lock, so its wait is for a worker); wait = probe latency minus its baseline p50. writers = non-request store holds during the arm (solo/contention).

## Reading

- **Plain store reads are short.** The handler's own read guard: p95
  0.35-12.6 ms, max 21.4 ms across both corpora and all six structural
  handlers. A concurrent call on another session waits p95 at most ~21 ms
  (B wait), max 45 ms.
- **The semantic rung is the outlier, and it runs under the lock.**
  `find_definition::handle` takes `store.read()` first and
  `by_semantic_neighbours` calls `embed_query` while that guard is held
  (`search_code::handle` embeds before taking the store). Warm: hold
  36-69 ms per call, of which `embed_query` is ~7 ms; the rest is the
  vector scan (`search_code::search`) under the guard. B waits p95 60-106 ms.
- **Cold (first semantic call after a daemon start):** `embed_query` takes
  ~570 ms (model load + inference), the hold 614-636 ms. For that whole time
  probe B (store) *and* probe C (`tools/list`, no store) stalled 616-639 ms:
  A occupies one worker holding the lock, B's blocked `Mutex::lock` occupies
  the other, so every session on the daemon, including calls that never
  touch the store, is frozen.
- **The same mechanism, smaller, for plain reads:** C waited p95 up to
  23 ms during contention arms. A worker parked on the store mutex is a
  worker lost to everyone, but bounded by the ~10-20 ms hold.
- **Shared rung:** `find_references`, `find_callers`/`find_callees` and
  `find_implementations` pass the embedding pipeline to the same name
  resolution, and a miss falls to `semanticNeighbours` there too (seen live:
  `find_callers("IndexStore::read")` on the g-mesh index answered
  `resolvedBy: semanticNeighbours`). They were measured with real names only,
  so their numbers above exclude the rung; a miss costs them what
  `find_definition_semantic` costs.

## Recommendation

**Leave the plain store reads on the worker; move the semantic rung off it,
and out from under the store lock.**

- Plain reads: hold p95 <= 12.6 ms, max 21 ms; cross-session wait p95 <=
  21 ms. Moving seven handlers to `spawn_blocking` would buy at most that
  tail, against changing every handler's call shape.
- Semantic rung: with 2 workers, the cold call freezes the whole daemon for
  ~0.6 s, and each warm call blocks other sessions' store reads for
  40-70 ms. Moving it to `spawn_blocking` alone (GM-448's pattern) frees the
  worker but not the lock: other sessions would still wait ~0.6 s on the
  store, and their waiting calls would still park the workers. So S2 should
  also embed **before** taking `store.read()`, as `search_code` does, and
  run the whole rung off the worker. That covers every name-anchored tool,
  since they share the rung.
- **What `find_definition`'s inference costs on the worker:** ~570 ms once
  per daemon (model load), then ~7 ms per semantic call; the vector scan
  under the lock adds 30-60 ms per call.

The owner decides.

## Reproduce

```sh
# throwaway worktree with the appendix patch applied, then:
cargo build --release --workspace --bins
(cd plugins/typescript && npm ci && npm run build)
BIN=<wt>/target/release PLUGINS=<wt>/plugins OUT=<dir> python3 eval/gm473_lock_hold.py run
cat <dir>/summary.md   # per-call raw data in <dir>/<corpus>.json
```

## Appendix: instrumentation patch (not committed to product code)

```diff
diff --git a/core/src/mcp/find_definition.rs b/core/src/mcp/find_definition.rs
index b7b693c..5ba4208 100644
--- a/core/src/mcp/find_definition.rs
+++ b/core/src/mcp/find_definition.rs
@@ -603,7 +603,14 @@ fn by_semantic_neighbours(
     if is_module_specifier(name) {
         return None;
     }
-    let query = embedding?.embed_query(name)?;
+    let t0 = std::time::Instant::now();
+    let query = embedding?.embed_query(name);
+    crate::storage::index_store::locklog(format_args!(
+        "EMBED {} 0 {} find_definition::by_semantic_neighbours",
+        crate::storage::index_store::locklog_now_us(),
+        t0.elapsed().as_micros()
+    ));
+    let query = query?;
     let page = super::search_code::search(conn, &query, SEMANTIC_CANDIDATES, None).ok()?;
     let results: Vec<DefinitionCandidate> = page
         .results
diff --git a/core/src/storage/index_store.rs b/core/src/storage/index_store.rs
index 3498a90..19f3d9a 100644
--- a/core/src/storage/index_store.rs
+++ b/core/src/storage/index_store.rs
@@ -46,6 +46,29 @@ thread_local! {
     static HELD: Cell<bool> = const { Cell::new(false) };
 }
 
+// GM-473 S1 throwaway instrumentation: with G_MESH_LOCKLOG set, every store
+// hold appends "kind t_acq_us wait_us hold_us site thread" to that file.
+static LOCKLOG: std::sync::OnceLock<Option<Mutex<std::fs::File>>> = std::sync::OnceLock::new();
+
+pub fn locklog(line: std::fmt::Arguments<'_>) {
+    use std::io::Write;
+    let sink = LOCKLOG.get_or_init(|| {
+        std::env::var_os("G_MESH_LOCKLOG").and_then(|p| {
+            std::fs::OpenOptions::new().create(true).append(true).open(p).ok().map(Mutex::new)
+        })
+    });
+    if let Some(f) = sink {
+        let th = std::thread::current();
+        let name = th.name().unwrap_or("?").replace(' ', "_");
+        let mut f = f.lock().unwrap_or_else(PoisonError::into_inner);
+        let _ = writeln!(f, "{line} {name}");
+    }
+}
+
+pub fn locklog_now_us() -> u128 {
+    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_micros()).unwrap_or(0)
+}
+
 /// Panics in a debug build if this thread holds the store. Called where a
 /// lock that must be taken before the store (see the module doc) is taken.
 pub fn assert_not_held() {
@@ -169,16 +192,21 @@ impl IndexStore {
     /// calls it; it uses the operations below. Keeps `Mutex::lock`'s shape
     /// and poisoning, so a test's `store.lock().unwrap()` reads as before.
     #[doc(hidden)]
+    #[track_caller]
     pub fn lock(&self) -> LockResult<StoreGuard<'_>> {
+        let site = std::panic::Location::caller();
+        let t0 = std::time::Instant::now();
         if HELD.with(Cell::get) {
             panic!(
                 "IndexStore re-entered: this thread already holds the store, and taking it again \
                  would self-deadlock"
             );
         }
-        match self.conn.lock() {
-            Ok(guard) => Ok(StoreGuard::new(guard)),
-            Err(poisoned) => Err(PoisonError::new(StoreGuard::new(poisoned.into_inner()))),
+        let r = self.conn.lock();
+        let wait = t0.elapsed();
+        match r {
+            Ok(guard) => Ok(StoreGuard::new(guard, site, wait)),
+            Err(poisoned) => Err(PoisonError::new(StoreGuard::new(poisoned.into_inner(), site, wait))),
         }
     }
 
@@ -190,17 +218,20 @@ impl IndexStore {
         self.conn.into_inner()
     }
 
+    #[track_caller]
     fn acquire(&self) -> StoreGuard<'_> {
         self.lock().unwrap()
     }
 
     /// The read path's guard, held for as long as the caller keeps it.
+    #[track_caller]
     pub fn read(&self) -> ReadGuard<'_> {
         ReadGuard(self.acquire())
     }
 
     /// One hold around `f`, for one-statement bookkeeping. `f` must not
     /// reach the store or a plugin-side lock.
+    #[track_caller]
     pub fn with<T>(&self, f: impl FnOnce(&Connection) -> T) -> T {
         f(&self.acquire())
     }
@@ -399,32 +430,50 @@ impl Writer<'_> {
 
 /// A held store. Clears the thread's held flag on drop.
 pub struct StoreGuard<'a> {
-    guard: MutexGuard<'a, Connection>,
+    guard: Option<MutexGuard<'a, Connection>>,
+    site: &'static std::panic::Location<'static>,
+    wait: std::time::Duration,
+    t_acq_us: u128,
+    acquired: std::time::Instant,
 }
 
 impl<'a> StoreGuard<'a> {
-    fn new(guard: MutexGuard<'a, Connection>) -> Self {
+    fn new(
+        guard: MutexGuard<'a, Connection>,
+        site: &'static std::panic::Location<'static>,
+        wait: std::time::Duration,
+    ) -> Self {
         HELD.with(|held| held.set(true));
-        Self { guard }
+        Self { guard: Some(guard), site, wait, t_acq_us: locklog_now_us(), acquired: std::time::Instant::now() }
     }
 }
 
 impl Drop for StoreGuard<'_> {
     fn drop(&mut self) {
+        let hold = self.acquired.elapsed();
+        drop(self.guard.take());
         HELD.with(|held| held.set(false));
+        locklog(format_args!(
+            "HOLD {} {} {} {}:{}",
+            self.t_acq_us,
+            self.wait.as_micros(),
+            hold.as_micros(),
+            self.site.file(),
+            self.site.line()
+        ));
     }
 }
 
 impl Deref for StoreGuard<'_> {
     type Target = Connection;
     fn deref(&self) -> &Connection {
-        &self.guard
+        self.guard.as_ref().unwrap()
     }
 }
 
 impl DerefMut for StoreGuard<'_> {
     fn deref_mut(&mut self) -> &mut Connection {
-        &mut self.guard
+        self.guard.as_mut().unwrap()
     }
 }
 
```
