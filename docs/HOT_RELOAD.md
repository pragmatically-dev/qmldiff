# Hot reload support

These exports let a host replace a loaded external diff at runtime, so a paired
extension (qt-resource-rebuilder) can re-register the affected Qt resources
without restarting the application. Loaded diffs are kept pristine and the merged
`CHANGES`/`SLOTS` are *derived* from them, so a replace re-resolves slots
deterministically (no stale or duplicated state, cross-file references included).

## Exports (FFI)

```c
// Replace a diff previously loaded under `id` (via qmldiff_add_external_diff)
// with new contents, in place. Returns false on a parse error (the old diff is
// left untouched). Safe to call after init, unlike qmldiff_add_external_diff.
bool  qmldiff_replace_external_diff(const char *contents, const char *id);

// The qmd -> qml binding: newline-joined qrc paths the diff `id` modifies, so a
// caller knows which resource roots to re-register. Free with qmldiff_free_string.
char *qmldiff_targets_of(const char *id);

void  qmldiff_free_string(char *pointer);
```

## How it works

Diffs are stored pristine (pre-slot-processing) in `RAW_DIFFS`, keyed by source
id. `ingest_external(id, contents, replace)` parses + version-filters the contents
and updates `RAW_DIFFS` (a replace drops prior entries for `id` first). The merged
state is then derived by `rebuild_changes()`:

1. clone every change from `RAW_DIFFS` (load order),
2. `update_slots` to strip + collect slot/template definitions,
3. if post-init, `process_slots` to expand all slot references across every file,
4. publish as `SLOTS` + `CHANGES`.

Because the merged state is a pure function of the loaded diffs, a replace is
always correct: no stale slots, no double-counting, no template-redefine panic,
and cross-file slot references re-resolve. The rebuild is lazy (a dirty flag +
`ensure_changes()` on each reader) so loading *n* diffs at startup stays O(n);
a live replace rebuilds immediately. The `POST_INIT -> SLOTS` lock order is
preserved.
