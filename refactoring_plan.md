# Refactoring plan

A list of architecture and code-organization improvements found by reviewing
all of `src/`. Each point is a self-contained task, ordered by value. Points 1
and 2 are the largest simplifications and make most of the others easier.

## General rules for every task

- Pure refactoring unless stated otherwise. Do not change the on-disk format,
  table names, relative paths, index values, the stats JSON consumed by
  `scripts/*.py`, CLI output, or HTTP responses. The tables in `~/ggegt/` must
  remain valid.
- Never modify `AGENTS.md`. Do not commit or create branches.
- Keep diffs focused: do not reformat code that the task does not touch. Run
  `rustfmt` only on lines you change (or not at all), so reviews stay readable.
- Keep comments minimal. Do not leave comments that describe the refactoring
  itself ("moved from", "formerly", "no longer").
- The network is blocked; use `--offline` with cargo.

### Standard validation

Run all of these after every task:

1. `cargo build --offline --all-targets`
2. `cargo clippy --offline --all-targets`: no new warnings (9 at the time of
   writing).
3. `cargo test --offline`: all pass, with a generous timeout. Change
   tests only where an API changed, without weakening assertions.
4. Report `git diff --stat`. Most tasks should reduce the line count.

---

## 1. Consolidate the table identity (done, including follow-ups)

**Done.** `TableId` (`src/table_id.rs`) is now a `Copy` value
`{ pawns: PawnKey, pieces: [[u8; 6]; 2] }`, with its name derived through
`Display` and the sort order unchanged. `Egt::new(&TableId)` builds the indexer
and `Egt` stores the `TableId`. `Egt::from_tablename` and `src/piece_set.rs`
were removed. The reader maps in `egt_file/batch.rs` and `egt_file/cache.rs` are
keyed by `TableId`, and probe routing (`position_table_name`) allocates nothing.

**Follow-up tasks:**

1. **Undo unrelated reformatting.** The previous change re-wrapped untouched
   code (`src/egt.rs` tests, `src/egt_file/cache.rs`, `src/generator/solver.rs`,
   and others). Restore the original formatting of lines whose content did not
   change, so the diff shows only real changes.
2. **Pass `&TableId` instead of names through the storage layer.**
   - `EgtFileReader::open_in(location, tablename: &str)` and
     `load_existing_file(.., endgame: &str)` (`src/egt_file.rs`) should take
     `&TableId`.
   - Then remove the `&id.tablename()` calls at their call sites
     (`egt_file/batch.rs`, `egt_file/cache.rs`, `verification.rs`,
     `egt_file/writer.rs::write_fixture`).
   - `EgtFileReader::new_from_file(&Path, &str)` may keep parsing a string,
     because it is the public entry point.
   - `EgtError` variants that need a name call `id.to_string()` only when the
     error is created.
3. **Remove the `String`-returning `tablename()` accessors.**
   - `TableId::tablename()` and `Egt::tablename()` now allocate on every call
     (about 35 non-test call sites).
   - Delete both and use `Display` (`format!("{id}")`, `println!("{}", id)`),
     or `name()` (a stack `ArrayString`) where a `&str` is needed for a
     comparison.
   - `EgtStats.tablename` stays a `String` field, because it is part of the
     JSON schema.
   - `Egt::id()` can return `&TableId` or `TableId`; choose one consistently.
4. **Compare `TableId`s by fields in `Ord`, without building names.**
   - `Ord for TableId` currently formats both names and then falls back to
     `elements()`, which allocates.
   - Derive an equivalent key directly from the fields: `(n_pieces, n_pawns)`,
     then per side the KQRBN counts in name order, then the pawn tokens (file
     letter, ignoring `o`), then a deterministic tie-break on the `PawnKey`.
   - Add a test asserting that the new order equals the old name-based order
     for every table in `resolve_selectors(&["2", "3", "4", "5"])`. Keep the
     old comparison as a test-only reference function.
   - The order is observable: it drives generation order, `--list` output and
     `GenerationPlan`.
5. **Remove the unused pawn slot in `pieces`.**
   - `pieces: [[u8; 6]; 2]` is indexed by `role as usize - 1`, and the pawn
     slot is always zero.
   - Use a 5-entry array (K, Q, R, B, N, in name order) with a single private
     index helper, or `ByRole<u8>` if that reads better.
   - Remove the `- 1` index arithmetic scattered in `table_id.rs`,
     `dependencies.rs` and `selection.rs` (`with_material` callers).
6. **Shrink the `EgtElement` surface.**
   - `EgtElement` now survives only as the indexer's input
     (`Indexer::from_elements`) plus test builders
     (`TableId::collect_elements`, the `#[cfg(test)]` `TableId::from_elements`
     and `Egt::from_elements`, and `validate_elements`).
   - Option A, preferred if shorter: let `Indexer` take a `&TableId` and
     iterate pawn files and piece counts directly, deleting `EgtElement`,
     `validate_elements` and the custom `Ord` (`TableId` construction already
     validates).
   - Option B: keep `EgtElement` private to `egt/indexer.rs`, produced by one
     `TableId` method, and merge the test builders into a single
     `#[cfg(test)]` helper.
   - Tests for noncanonical orientations (`decode_encode_kpdpe_kpepepe` and
     similar in `src/egt.rs`) can use `parse_oriented`, which already exists,
     instead of hand-built elements.
7. **Small leftovers.**
   - `TableId::from_position` and `position_table_name` are duplicates: keep
     one (`TableId::from_position`).
   - `pub(crate) fn with_material` is only needed by `selection.rs`; consider
     a clearer name such as `TableId::new(pawns, pieces)`.
   - `material()` duplicates the piece loop of `format_name`; express it
     through a shared helper or the formatted name without lowercase letters.

---

## 2. Split `EgtFileReader` into file I/O and a frame store (done)

**Problem.** `src/egt_file.rs::EgtFileReader` mixes three jobs:
- storage I/O (seek table, range reads);
- a resident-frame cache (`frames: Vec<Option<Frame>>`);
- indexing pass-throughs (`map_position_to_index`, `index_to_position`).

It also offers three access styles: `&mut` loading, `*_cached` returning
`Option`, and the near-duplicates `frame_chunk` / `frame_chunk_cached`.
`egt_file/cache.rs` and `egt_file/batch.rs` reach into private fields
(`file.frames[..]`, `load_all`). This forces `SharedFileCache` to use
`Arc<RwLock<EgtFileReader>>`, LRU keys built from `Arc::as_ptr` addresses, and
a read-to-write lock upgrade with a recheck. `BatchReader` carries "cold frame
in prepared dependencies" `Internal` errors for a state that cannot occur.

**Implementation.**
1. Rename and reduce the reader to an immutable
   `pub(crate) struct TableFile { egt: Egt, location: Location, key: ObjectPath, seek_table: SeekTable }`
   with:
   - `open(location, &TableId) -> EgtResult<Self>`
   - `num_frames()`, `frame_len(frame)`, `locate(index) -> (frame, offset)`
   - `read_frames(&self, frames: &[usize]) -> EgtResult<Vec<Frame>>`: the
     existing multi-range logic, including the truncation diagnosis.
   - `read_all(&self) -> EgtResult<Vec<Frame>>`: parallel, one decode per
     worker, as `load_all` does today.
   - `index(&self, position, reflected) -> EgtResult<usize>`: the current
     `map_routed_position_to_index`.

   It has no mutable state. `Frame` stays in `egt_file/frame.rs`.
2. `BatchReader` (`egt_file/batch.rs`) stores
   `HashMap<TableId, (TableFile, Vec<Frame>)>`, where every frame is always
   loaded.
   - `open` loads everything eagerly; the separate `load()` step can disappear.
   - `insert` takes an already-loaded table.
   - `probe`, `probe_outcome` and `read_chunk` index straight into frames,
     without `Option` and without `Internal` errors.
   - `resident_bytes` sums `Frame::bytes`.
   - Callers in `generator.rs` and `verification.rs` are updated accordingly.
     `verification.rs` currently opens the source separately and `insert`s it;
     keep that "explicit output overrides the input overlay" behavior.
3. `SharedFileCache` (`egt_file/cache.rs`):
   - `files: RwLock<HashMap<TableId, Arc<TableFile>>>`: metadata only,
     immutable, shared.
   - `frames: Mutex<Lru>` where
     `Lru { map: HashMap<(TableId, usize), (Arc<Frame>, u64 last_used)>, resident, assigned, clock }`.
   - Probing: route the position, get or open the `TableFile`, compute the
     indices, then look up the frames under the mutex. Read missing frames
     *outside* the mutex with `read_frames`, then insert them (if a racing
     thread inserted first, keep either one). Evict the least recently used
     frames until the budget is met. Copy outcomes out of the `Arc<Frame>`s,
     so a zero budget still works.
   - Delete `FrameKey` with pointer addresses, `CachedFrame.file`,
     `forget_file`, the lock-upgrade paths, and most "lock poisoned" `Internal`
     errors (use `.lock().unwrap()` / `expect`; see point 8).
   - Keep a single batched entry point
     `probe_outcomes(&[Chess]) -> EgtResult<Vec<DtcOutcome>>`. `probe(position)`
     calls it with a one-element slice. Delete `probe_direct_outcome`,
     `probe_routed`, and the `#[cfg(test)]`-only `probe`, `probe_outcome`,
     `probe_indexed`, `insert`, `remove` and `is_empty` unless a test genuinely
     needs them; rewrite those tests against the public behavior.
   - `set_input_path` / `clear` stay.
4. Delete `EgtFileReader::frame_chunk`, `frame_chunk_cached`,
   `read_from_index`, `read_from_index_cached`, `read_indices_cached`,
   `get_frame_data`, `ensure_uncompressed_frames`, `probe` and `probe_cached`
   if they are no longer used. The public `EgtFileReader` export in `lib.rs`
   goes away; it is only used in `tests.rs`, which can use `TableFile`
   in-crate.
5. Tests: keep the cache-behavior coverage in `egt_file/cache.rs`
   (cold/warm probes, LRU eviction with short frames, zero budget, a missing
   file reported as an error) and the corrupt-file tests in `egt_file.rs`.

**Expected outcome:** `egt_file/cache.rs` roughly halves. No `RwLock` per file.
No `Option<Frame>` anywhere.

---

## 3. Put the input-path fallback in one place (done)

**Problem.** `EgtGenerator` (`generator.rs`), `EgtVerifier` (`verification.rs`)
and `EgtProber` (`prober.rs`, through `SharedFileCache`) each carry
`base_path` + `input_path`, identical `new` / `from_location` /
`set_input_path` code, and (two of them) `threads`. The rule "input first,
base only if missing" lives in the free function
`egt_file::load_existing_file`, and `BatchReader::open` re-wraps the errors as
`DependencyUnavailable`.

**Implementation.**
1. Add `pub(crate) struct Source { base: Location, input: Option<Location> }`
   in `storage.rs` (or `egt_file.rs`) with:
   - `Source::new(base)` and `with_input(..)`
   - `open(&self, id: &TableId) -> EgtResult<TableFile>`, which applies the
     fallback rule.
   - `base()`, the publication target used by the generator.
2. Replace `load_existing_file` with `Source::open`. The generator, the
   verifier and the cache hold a `Source`.
3. The facades keep their public methods (`new`, `set_input_path`,
   `set_threads`), each now a one-liner. Optionally, put the shared
   `threads` + pool construction in a small helper
   `fn pool(threads) -> EgtResult<ThreadPool>` (duplicated today in
   `generator.rs` and `verification.rs`).
4. Optional larger step, only if the user agrees, because it changes the
   public API: merge the facades into one `Tablebase` handle with `generate`,
   `verify`, `probe`, `probe_with_moves`, `stats` and `cache_stats`. Do not do
   this without confirmation.

---

## 4. Shrink and clean up the public API (`src/lib.rs`) (done)

Point 3 introduced `Tablebase`, consolidated generation and verification into
one slice-based entry point each, and documented the blocking API. The
remaining work is to narrow the exports around that API.

**Implementation.**
1. Remove the unused `print_pair_stats` in `src/generator.rs`. Keep
   `print_table_stats` private and preserve its output.
2. Remove the public re-exports of `PackedOutcome`, `GenerationPlan`,
   `TablePair`, and `TableSelector` from `src/lib.rs`. Update internal imports
   to use their defining modules and narrow visibility where possible. Make
   `mod error` private but retain the `EgtError` and `EgtResult` re-exports.
   Make `quiet_unmoves` private to `src/generator/solver.rs`; its current
   callers are all in that module, including its tests.
3. Move `ProbeWithMoves` next to `Tablebase::probe_with_moves` in
   `src/tablebase.rs` and keep re-exporting it at the crate root.
4. The intended crate-root exports are `Tablebase`, `ProbeWithMoves`,
   `TableId`, `DtcOutcome`, `ConversionType`, `EgtStats`, `AggregateStats`,
   `LongestDtcPosition`, `EgtError`, `EgtResult`, and `resolve_selectors`.
   Decide separately whether to keep `Egt` public for low-level index access;
   neither binary uses it.
5. Optional: keep the current `&[&str]` selector signatures unless accepting
   borrowed strings directly simplifies the API and callers. If changing them,
   consider `generate`, `verify`, `stats`, `cache_stats`, and
   `resolve_selectors` together: their CLI call sites all collect `Vec<&str>`.
   Preserve selector semantics and account for empty selections, such as the
   server's `stats(&[])` startup check.
6. Keep the crate docs and example aligned with the final exports and run
   `cargo test --doc --offline`.

---

## 5. Reorganize the generator (done)

**Problem.** `src/generator/solver.rs` (about 800 lines of code and 850 of
tests) mixes several things:
- quiet-unmove generation (pure chess logic);
- the move-counter symmetry adjustment;
- the working arrays and `PlySummary`;
- initialization and propagation;
- `generate_pair` orchestration and timing output.

In addition:
- `src/generator.rs` has `print_table_stats` / `print_pair_stats` after its
  test module (a clippy warning).
- `WorkingPair { values: Vec<Vec<AtomicU16>> }` and `TableState` are parallel
  structures linked by slot index.
- `[_; 2]` arrays meaning "0 = win, 1 = loss" (`maxima`, `histograms`,
  `candidates` in `finalization/stats.rs`) are hard to read.

**Implementation.**
1. Create `src/generator/unmoves.rs` with `quiet_unmoves`,
   `pawnless_quiet_unmoves`, `pawnful_quiet_unmoves`,
   `emit_pawnful_predecessors`, `kings_on_long_diagonal`,
   `both_kings_on_diagonal` and `symmetry_adjusted_move_counter`, plus their
   tests (all `*unmoves*` tests in `solver.rs`). Move the "Move counters and
   diagonal symmetry" doc section with them, and update the README link
   (`README.md`, "Move counters" section, which references
   `src/generator/solver.rs`).
2. Move `values: Vec<AtomicU16>` into `TableState`, and delete `WorkingPair`.
   `mark_win` and `decrement` become free functions or methods on
   `&[AtomicU16]` (e.g. a small `struct Cells<'a>(&'a [AtomicU16])` or
   extension functions). `scan_table` takes the source `TableState` and the
   target `TableState`. A self-twin remains `tables.len() == 1` with
   `twin_slot == slot`. The allocation with `try_reserve_exact` and the
   `WorkingMemoryAllocation` error must be kept (put it in `TableState::new`).
   `PairResult` then only carries the timings, or disappears.
3. Move `generate_pair` from `solver.rs` into `generator.rs`, so that
   `solver.rs` exposes only `solve_pair(tables, reflected, reader)`.
   `generator.rs` holds the facade plus the per-pair orchestration: load
   dependencies, solve, finalize, report.
4. Add `#[derive(Default, Clone, Copy)] struct WinLoss<T> { win: T, loss: T }`
   (in `generator.rs`) and use it for `maxima`, the `Found`-style counters,
   and `TableStats.histograms` / `candidates`. `finalize_table` takes
   `WinLoss<u16>` instead of `[u16; 2]`.
5. Move the reporting functions (`print_table_stats`, `generation_summary`)
   into the reporting module from point 6 and place them before
   `mod tests`.
6. Tests: the existing `solver.rs` tests move with their code. The
   end-to-end generation tests (`test_*_table_generation`) stay.

---

## 6. Simplify statistics (done)

**Problem.** `src/stats.rs` has five types:
- `EgtStats`;
- `AggregateStats`, with 14 `Option` fields enumerated one by one in `add`,
  `add_record`, `merge`, `validate` and `Default`;
- `PlaceholderStats`, with unit `()` fields serialized as `null`;
- `StatsRecord`, untagged, with a custom `TryFrom<Value>`;
- `required_nullable`.

The "all null or none null" rule is a runtime check. The generator adds a
sixth type, `generator/finalization/stats.rs::TableStats`, whose name clashes
with `crate::stats`. `format_timing` lives in `stats.rs` but is reporting.

**JSON compatibility constraint.** `scripts/export_table_stats.py`,
`export_summary_stats.py` and `stats_csv.py` read flat fields (`bytes`,
`unique_positions`, `win`, `draw`, `loss`, `index_range`, `tablename`,
`hierarchy`, `stats`) and treat `"bytes": null` as a missing table. The
serialized JSON must stay identical, or the scripts and existing sidecars in
`~/ggegt/` must be updated in the same task. Prefer keeping it identical by
using `#[serde(flatten)]`.

**Implementation.**
1. Add `#[derive(Default, Clone, PartialEq, Eq, Serialize, Deserialize)] pub struct Totals`
   with the 14 counter fields (`bytes`, `num_frames`, `unique_positions`,
   `win`, `draw`, `loss`, the three `invalid_*`, `redundant`, the two
   histograms, the two `longest_dtc_*`) and one `fn add(&mut self, other: &Totals)`
   containing the histogram/example merge logic (`merge_distribution`), plus
   one `fn validate(&self, index_range) -> EgtResult<()>` (`validate_outcomes`
   + `validate_distribution`).
2. `EgtStats = { tablename, sha256, index_range, frame_size, #[serde(flatten)] totals: Totals }`.
   Watch field order: serde emits flattened fields in place, so check that
   the JSON matches the old layout byte for byte. If the order matters for
   the tests, keep the declaration order.
3. `AggregateStats = { selected_tables, available_tables, index_range, #[serde(flatten)] totals: Option<Totals> }`.
   Flattening an `Option` struct cannot produce "every field null". So:
   - either implement `Serialize` / `Deserialize` manually, emitting each
     `Totals` field as `null` when absent (a short custom impl through a
     helper struct with `Option` fields generated by one macro);
   - or, if the user accepts a schema change, serialize `totals` as a nested
     object or `null`, and update `export_summary_stats.py` and all cached
     `stats.json` (rebuild them with `--cache-stats`).

   Ask the user before choosing the schema-changing option.
4. Placeholders: per-table `.json` placeholders (`"bytes": null`) exist only
   so that `--cache-stats` can record theoretical tables, and the scripts rely
   on them. Keep the on-disk placeholder format, but represent it in Rust as
   `Option<EgtStats>` alongside `{ tablename, index_range }`; for example,
   `StatsRecord { tablename, index_range, stats: Option<EgtStats> }` with the
   same serialized shape. This removes `PlaceholderStats` and the
   `TryFrom<Value>` trick.
5. Rename `generator/finalization/stats.rs::TableStats` to `StatsAccumulator`.
   It can stay in `generator/finalization/` but should build `Totals`
   directly.
6. Create `src/report.rs` (crate-private) with `format_timing`,
   `generation_summary` and `print_table_stats`, the progress/summary
   formatting now spread over `stats.rs` and `generator.rs`.
7. Make sure the stats tests in `stats.rs` and `stats/cache.rs` still pass,
   and that `--stats` / `--cache-stats` output on `~/ggegt/` is unchanged
   (standard validation step 4).

---

## 7. Use one hierarchy type

**Problem.** The `<n>/<max>v<min>/op<K>/<N>pawns` hierarchy is handled in
four places:
- formatted in `TableId::relative_path`;
- parsed in `selection::Prefix` (`TableSelector::parse`);
- matched by two near-identical functions (`Prefix::matches` and
  `Prefix::matches_material`);
- split by string in `stats/cache.rs::ancestors`.

**Implementation.**
1. Add `pub(crate) struct Hierarchy { n: usize, split: Option<(usize, usize)>, opposing: Option<usize>, pawns: Option<usize> }`
   (essentially today's `Prefix` with `n` required) in `selection.rs` or a
   new `src/hierarchy.rs`, with:
   - `parse(&str)`: move the validation from `TableSelector::parse`, keeping
     the same error messages.
   - `Display`, which produces the canonical prefix with a trailing `/`.
   - `ancestors(&self) -> impl Iterator<Item = Hierarchy>`.
   - `contains(&self, id: &TableId) -> bool`.
2. Add `TableId::hierarchy() -> Hierarchy` (fully specified), and make
   `relative_path` `format!("{}{}.{}", self.hierarchy(), self, ext)`.
3. `stats/cache.rs`: replace the string `ancestors` and the
   `format!("{prefix}stats.json")` calls with `Hierarchy` values (`Summary`
   still stores the string form for JSON compatibility).
   `selection::hierarchy_prefixes` returns `Vec<Hierarchy>`, so its
   "covered descendant" deduplication becomes a `contains` check.
4. Replace `Prefix::matches_material` with a check on the material's piece
   counts only, or drop it, keeping `Hierarchy::contains` on the generated
   `TableId`s (it is just a cheap pre-filter; measure that `--list 8/` does
   not become much slower).
5. Move the selection-only helpers `get_file_combinations`,
   `pawn_order_combinations` and `is_canonical` / `compare_file_reflection`
   from `table_id.rs` to `selection.rs` (keep `PawnKey::canonicalized` in
   `table_id.rs`, which also uses the comparison; move only the helper if it
   stays shared).
6. Replace `GenerationPlan { pairs }` with
   `pub(crate) fn plan_pairs(ids) -> Vec<TablePair>`.

---

## 8. Tighten errors (`src/error.rs`)

**Problem.**
- `EgtError::Internal` appears 47 times: 22 in `egt_file/cache.rs`, mostly
  for lock poisoning, and some impossible cases such as "position_opt should
  be Some after the None check" in `verification.rs`.
- `position::position_from_setup` returns
  `EgtResult<Result<Chess, InvalidPosition>>`, where the outer error is only
  a broken invariant.
- There are two kinds of "bad data" errors: `InvalidData { location }` and
  `Io(io::ErrorKind::InvalidData)` (`stats.rs::invalid_data` and
  `frame.rs::invalid`).
- `PackedOutcome::to_outcome_at(index)` mixes decoding with error context.

**Implementation.**
1. Lock poisoning: use `.lock().unwrap()` / `.read().unwrap()` (a poisoned
   lock means another thread panicked). Broken invariants: `expect("...")` or
   `debug_assert!`, or restructure the code (in `verify_block`, `match` on the
   `Option` instead of checking and then calling `ok_or`). Keep `Internal`
   only where returning an error is genuinely better than panicking (for
   example, writer segment validation in `egt_file/writer.rs`), or remove the
   variant entirely if none remain.
2. Make `position_from_setup` return `Result<Chess, InvalidPosition>`, and
   turn the "EP target has no geometric pawn capturer" case into a
   `debug_assert!`. Update `Egt::position_from_index_with_reason` and the
   solver's `initialize_table` accordingly.
3. Use a single bad-data representation: stats and frame decoding errors
   become `EgtError::InvalidData` (the location can be added by the caller,
   as `EgtFileReader::invalid_frame` and `Location::invalid_data` already do).
   Remove the `Io(InvalidData)` constructions.
4. Replace `to_outcome_at(index)` with
   `impl TryFrom<PackedOutcome> for DtcOutcome` (error type: the raw `u16`).
   Callers that need context map the error to
   `EgtError::CorruptedOutcome { value, index }`.
5. Fix the broken indentation of lines 43–51 of `error.rs` and add doc
   comments to the two undocumented variants (`InvalidStorageLocation`,
   `Storage`).
6. Check whether `DependencyUnavailable`, `PositionNotInTable`,
   `IndexOutOfRange` and `FileNotFound` are all still produced after points 2
   and 3, and delete the dead ones (the enum is `#[non_exhaustive]`).

---

## 9. Smaller indexing simplifications

**Implementation.**
1. `Egt` (`src/egt.rs`) is mostly delegation to `Indexer`
   (`src/egt/indexer.rs`), and both store `n_pieces` / `n_pawns`. Either:
   - make `Indexer` fields `pub(super)` and drop the duplicate fields on
     `Egt`, delegating everything; or
   - preferred: merge them, so that `Egt { id, plan: IndexPlan }` keeps the
     methods in `egt.rs` and only the algorithmic helpers stay in
     `indexer.rs`.

   Choose whichever produces less code. Keep the large module doc in
   `indexer.rs`.
2. Reduce the three decode entry points (`position_from_index`,
   `position_from_index_with_reason`, `position_from_index_unchecked`, plus
   the `const VALIDATE: bool` generic) to two:
   - `decode(index, turn) -> Result<Chess, InvalidPosition>` (index range
     checked by `debug_assert` or by returning an extra variant);
   - `decode_unchecked(index, turn) -> Chess`.

   The current `position_from_index(..) -> Option<Chess>` callers use
   `.ok()`. Out-of-range handling: callers that can pass an out-of-range
   index (public API, tests) check `index < index_range()` first.
3. Replace `filled_coord(n)` / `filled_pidx(n)` with fixed-size stack arrays
   `[(usize, usize); MAX_PIECES]` / `[usize; MAX_PIECES]` and slice them to
   `n_pieces`, or keep `ArrayVec` but build them with `ArrayVec::from` /
   `.extend(repeat_n(..))`. Benchmark: run
   `--generate 4/ --threads 2 --path /tmp/x` on a copy of the tables, or
   `cargo test --release decode_encode`, and check that it is not slower.
4. Fix the clippy warnings in `indexer.rs` (the `N_CHOOSE_K` loop index) only
   where the result reads better.

---

## 10. Binaries (`src/bin/`)

**Implementation.**
1. Inline `src/bin/common/mod.rs` (`parse_byte_size`) into
   `src/bin/server.rs`, and delete the `common` directory.
2. Add `impl ConversionType { pub fn as_str(self) -> &'static str }`
   (`"checkmate"`, `"capture"`, `"promotion"`) in `outcome.rs`, and use it in
   `main.rs::format_outcome` and `server.rs::OutcomeResponse::new`.
3. FEN parsing with `ignore_impossible_check` is duplicated in `main.rs` and
   `server.rs::parse_position`. Add a library helper
   `pub fn parse_position(fen: &str) -> EgtResult<Chess>` (e.g. in
   `prober.rs`, returning `EgtError::InvalidPosition`) and use it in both
   places (the server maps the error to its `ApiError`).
4. Optional, and it changes the CLI (ask first): switch `main.rs` to clap
   subcommands (`chess-egt generate 3/ 4/`, `chess-egt verify ..`,
   `chess-egt stats ..`, `chess-egt cache-stats ..`, `chess-egt list ..`,
   `chess-egt probe FEN`), with `--path`, `--input-path` and `--threads` as
   global or per-subcommand options. This removes the `ArgGroup`, the five
   `Vec<String>` fields and the FEN/selector-ordering tests. Update the
   README "Usage" section and `scripts/benchmark_generation.py` if it calls
   the CLI.

---

## 11. Verification (`src/verification.rs`)

**Implementation.**
1. Compute the successors once per position:
   `let moves: Vec<(Move, DtcOutcome)> = legals.map(|m| (m, reader.probe_outcome(&play(m))?))`.
   Then `best = moves.iter().map(|(m, o)| o.before_move(m)).max()`, and on a
   mismatch `describe_mismatch(&position, &moves, outcome, best)` formats the
   same list without probing again. Delete `best_outcome`.
2. In `verify_block`, remove the `index = local_idx` alias and the
   `position_opt` check-then-`ok_or(Internal)` step: `match` on the decode
   result once.
3. `verify_tables_in_pool` builds the dependency list with
   `id.dependencies()` + twin − self; keep it, but use the `Source` (point 3)
   and the eager `BatchReader` (point 2).

---

## 12. Formatting and comments

**Implementation.**
1. Run `cargo fmt` over the whole crate in a dedicated commit-sized step
   (only after the structural tasks, to avoid noisy diffs during them), then
   fix the remaining clippy warnings where the fix reads better
   (`collapsible_if`, `is_multiple_of`, `items_after_test_module`,
   `large_enum_variant` on `StatsRecord` may disappear with point 6,
   `too_many_arguments` in tests).
2. Remove or shorten comments that describe how the code came to be, or that
   repeat the code. Known examples:
   - "copied from the former storage-layer identity" (`PawnKey`; already
     partly removed);
   - "Warm probes borrow this stack name; only cache misses need an owned
     identity";
   - repeated "without constructing an indexing plan", "never constructs full
     Egt indexing plans", "no storage I/O", "performs no storage I/O"
     (`table_id.rs`, `selection.rs`).

   Use the `claudy-comment-detector` skill for a systematic pass.
3. Rename test functions with plan-step prefixes: `step4_*`, `pair_local_*`
   in `generator/solver.rs` become descriptive names.
4. README typos: "Is is not intended" → "It is not intended"; "the the
   50-move rule" → "the 50-move rule". After points 2, 5 and 6, update the
   README "High-level overview" and "Storage and memory layer" sections to
   the new type names (`TableFile`, `SharedFileCache`, `solve_pair`
   location). Do not touch `AGENTS.md`, even though it mentions
   `EgtFileReader`; tell the user instead.
