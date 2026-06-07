#![allow(dead_code)]
use file_id::FileId;
use hashrules::HashRules;
use hashtab::{merge_hash_file, serialize_hashtab, HashTab};
use lazy_static::lazy_static;
use lib_util::{include_if_building_hashtab, is_building_hashtab};
use parser::diff::parser::{Change, ObjectToChange};
use processor::find_and_process;
use slots::Slots;
use std::collections::HashSet;
use std::ops::Deref;
use std::os::raw::c_void;
use std::time::Duration;
use std::{
    ffi::{c_char, CStr, CString},
    sync::Mutex,
};
use util::common_util::{load_diff_file, parse_diff};

use crate::parser::diff::parser::ExternalLoader;
use crate::util::common_util::{filter_out_non_matching_versions, tokenize_qml};

mod hash;
mod hashrules;
mod hashtab;
mod parser;
mod processor;
mod refcell_translation;
mod slots;

#[path = "util/lib_util.rs"]
mod lib_util;
mod util;

type CExternalLoaderFunc = unsafe extern "C" fn(file_name: *const c_char) -> c_void;

lazy_static! {
    static ref HASHTAB: Mutex<HashTab> = Mutex::new(HashTab::new());
    static ref SLOTS: Mutex<Slots> = Mutex::new(Slots::new());
    static ref CHANGES: Mutex<Vec<Change>> = Mutex::new(Vec::new());
    static ref POST_INIT: Mutex<bool> = Mutex::new(false);
    static ref HASHTAB_RULES: Mutex<Option<HashRules>> = Mutex::new(None);
    static ref CURRENT_VERSION: Mutex<Option<String>> = Mutex::new(None);
    static ref SLOTS_DISABLED: Mutex<bool> = Mutex::new(false);
    static ref EXTERNAL_LOADER: Mutex<Option<CExternalLoaderFunc>> = Mutex::new(None);
    static ref SEEN_FILES: Mutex<HashSet<FileId>> = Mutex::new(HashSet::new());
    // Pristine, pre-slot-processing changes per source ("" key = file-loaded base),
    // kept in load order. SLOTS + CHANGES are derived from these by rebuild_changes,
    // so adding or replacing a diff re-resolves all slots deterministically.
    static ref RAW_DIFFS: Mutex<Vec<(String, Vec<Change>)>> = Mutex::new(Vec::new());
    // Set when RAW_DIFFS changes; the merged CHANGES/SLOTS are rebuilt lazily on
    // next read, so loading N diffs at startup stays O(N) (one rebuild), not O(N^2).
    static ref CHANGES_DIRTY: Mutex<bool> = Mutex::new(false);
}

/// Rebuild the merged CHANGES/SLOTS if RAW_DIFFS changed since the last build.
/// Cheap no-op when clean; called at the start of every reader.
fn ensure_changes() {
    let dirty = {
        let mut d = CHANGES_DIRTY.lock().unwrap();
        let was = *d;
        *d = false;
        was
    };
    if dirty {
        rebuild_changes();
    }
}

/// Rebuild SLOTS and CHANGES from scratch out of the pristine RAW_DIFFS. This is
/// the single source of truth for the merged change set: it re-derives the slot
/// table and (post-init) re-expands every slot reference across *all* files, so a
/// live `qmldiff_replace_external_diff` cannot leave stale, duplicated, or
/// cross-file-unresolved slot state behind. Idempotent.
fn rebuild_changes() {
    let mut all: Vec<Change> = RAW_DIFFS
        .lock()
        .unwrap()
        .iter()
        .flat_map(|(_, changes)| changes.iter().cloned())
        .collect();
    // Read POST_INIT before locking SLOTS to keep the POST_INIT->SLOTS lock order.
    let post_init = *POST_INIT.lock().unwrap();
    let mut slots = Slots::new();
    slots.update_slots(&mut all); // strips slot/template definitions, fills `slots`
    if post_init {
        // Slots were already sealed during normal init, so expand references now.
        slots.process_slots(&mut all);
    }
    *SLOTS.lock().unwrap() = slots;
    *CHANGES.lock().unwrap() = all;
}

#[no_mangle]
unsafe extern "C" fn qmldiff_set_external_loader(external_loader: CExternalLoaderFunc) {
    *EXTERNAL_LOADER.lock().unwrap() = Some(external_loader);
}

#[no_mangle]
unsafe extern "C" fn qmldiff_set_version(version: *const c_char) {
    *CURRENT_VERSION.lock().unwrap() = Some(CStr::from_ptr(version).to_str().unwrap().into());
    eprintln!(
        "[qmldiff]: Set system version to {}",
        (*CURRENT_VERSION.lock().unwrap()).as_ref().unwrap()
    );
}

#[no_mangle]
extern "C" fn qmldiff_load_rules(rules: *const c_char) {
    let rules: String = unsafe { CStr::from_ptr(rules) }.to_str().unwrap().into();
    match HashRules::compile(&rules) {
        Ok(rules_ok) => {
            *HASHTAB_RULES.lock().unwrap() = Some(rules_ok);
            eprintln!("[qmldiff]: Configured hashtab rules.");
        }
        Err(error) => {
            eprintln!("[qmldiff]: Error loading rules: {}", error);
        }
    }
}

// Shared ingest path for external diffs (both initial add and live replace).
// Parses `contents` under the identifier `id`, version-filters, and registers
// its slots. When `replace` is set, any previously-loaded changes from the same
// `id` are dropped first (matched on `Change::source`, which equals `id`). After
// init we must process slots here, since the normal seal-on-first-process has
// already happened and would otherwise leave a live replacement's slots unfilled.
fn ingest_external(id: &str, contents: &str, replace: bool) -> anyhow::Result<usize> {
    let mut parsed = parse_diff(
        None,
        contents.to_string(),
        id,
        &HASHTAB.lock().unwrap(),
        None,
        None, // External diffs are exempt from seen-files checking.
    )?;
    filter_out_non_matching_versions(&mut parsed, CURRENT_VERSION.lock().unwrap().clone(), id);
    let count = parsed.len();
    {
        // Store the pristine changes keyed by id; a replace drops prior entries
        // for this id first. SLOTS/CHANGES are derived by rebuild_changes below.
        let mut raw = RAW_DIFFS.lock().unwrap();
        if replace {
            raw.retain(|(existing, _)| existing != id);
        }
        raw.push((id.to_string(), parsed));
    }
    *CHANGES_DIRTY.lock().unwrap() = true;
    // A replace is a live hot-reload: rebuild now so the change takes effect
    // immediately. An add at startup defers to the first reader (kept O(N)).
    if replace {
        ensure_changes();
    }
    Ok(count)
}

#[no_mangle]
extern "C" fn qmldiff_add_external_diff(
    change_file_contents: *const c_char,
    file_identifier: *const c_char,
) -> bool {
    if is_building_hashtab() {
        return false;
    }

    let file_identifier: String = unsafe { CStr::from_ptr(file_identifier) }
        .to_str()
        .unwrap()
        .into();

    if *POST_INIT.lock().unwrap() {
        eprintln!(
            "[qmldiff]: Cannot build changes from external {} after init has completed!",
            &file_identifier
        );
    }
    let change_file_contents: String = unsafe { CStr::from_ptr(change_file_contents) }
        .to_str()
        .unwrap()
        .into();
    match ingest_external(&file_identifier, &change_file_contents, false) {
        Ok(_) => {
            eprintln!("[qmldiff]: Loaded external {}", &file_identifier);
            true
        }
        Err(problem) => {
            eprintln!(
                "[qmldiff]: Failed to load external {}: {:?}",
                &file_identifier, problem
            );
            false
        }
    }
}

/// Hot reload: replace a previously-loaded external diff in place. Drops the old
/// changes for `file_identifier` and ingests the new contents under the same id.
/// Pair this with `qrr_reload_external_diff` in qt-resource-rebuilder, which
/// re-registers the affected Qt resource roots afterwards.
///
/// # Safety
/// `change_file_contents` and `file_identifier` must be valid C strings.
#[no_mangle]
pub unsafe extern "C" fn qmldiff_replace_external_diff(
    change_file_contents: *const c_char,
    file_identifier: *const c_char,
) -> bool {
    if is_building_hashtab() {
        return false;
    }
    let file_identifier: String = CStr::from_ptr(file_identifier).to_str().unwrap().into();
    let change_file_contents: String = CStr::from_ptr(change_file_contents)
        .to_str()
        .unwrap()
        .into();
    match ingest_external(&file_identifier, &change_file_contents, true) {
        Ok(count) => {
            eprintln!(
                "[qmldiff]: Reloaded external {} ({} change(s))",
                &file_identifier, count
            );
            true
        }
        Err(problem) => {
            eprintln!(
                "[qmldiff]: Failed to reload external {}: {:?}",
                &file_identifier, problem
            );
            false
        }
    }
}

/// The qmd -> qml binding: a newline-joined list of the qrc paths a given
/// external diff targets, so the resource rebuilder knows exactly which roots to
/// re-register on a hot reload. The returned string is heap-allocated; free it
/// with `qmldiff_free_string`.
///
/// # Safety
/// `file_identifier` must be a valid C string.
#[no_mangle]
pub unsafe extern "C" fn qmldiff_targets_of(file_identifier: *const c_char) -> *mut c_char {
    ensure_changes();
    let file_identifier: String = CStr::from_ptr(file_identifier).to_str().unwrap().into();
    let changes = CHANGES.lock().unwrap();
    let mut targets: Vec<String> = changes
        .iter()
        .filter(|change| change.source.as_str() == file_identifier.as_str())
        .filter_map(|change| match &change.destination {
            ObjectToChange::File(path) | ObjectToChange::FileTokenStream(path) => Some(path.clone()),
            _ => None,
        })
        .collect();
    targets.sort();
    targets.dedup();
    CString::new(targets.join("\n")).unwrap().into_raw()
}

/// Free a string returned by `qmldiff_targets_of`.
///
/// # Safety
/// `pointer` must be null or a value previously returned by `qmldiff_targets_of`.
#[no_mangle]
pub unsafe extern "C" fn qmldiff_free_string(pointer: *mut c_char) {
    if !pointer.is_null() {
        drop(CString::from_raw(pointer));
    }
}

fn load_hashtab(root_dir: &str) {
    let mut hashtab = HASHTAB.lock().unwrap();
    if let Err(x) = merge_hash_file(
        std::path::Path::new(&root_dir).join("hashtab"),
        &mut hashtab,
        CURRENT_VERSION.lock().unwrap().clone(),
        None,
    ) {
        eprintln!("[qmldiff]: Failed to load hashtab: {}", x);
    } else {
        println!(
            "[qmldiff]: Hashtab loaded! Cached {} entries",
            hashtab.len()
        );
    }
}

impl ExternalLoader for CExternalLoaderFunc {
    fn load_external(&mut self, file: &str) {
        let c_string = CString::new(file).unwrap();
        unsafe {
            self(c_string.as_ptr());
        }
    }
}

#[no_mangle]
extern "C" fn qmldiff_build_change_files(root_dir: *const c_char) -> i32 {
    if is_building_hashtab() {
        return 0;
    }

    let root_dir: String = unsafe { CStr::from_ptr(root_dir) }.to_str().unwrap().into();

    if *POST_INIT.lock().unwrap() {
        eprintln!(
            "[qmldiff]: Cannot build changes from {} after init has completed!",
            &root_dir
        );
    }
    let mut loaded_files = 0i32;
    let mut all_changes = Vec::new();

    eprintln!("[qmldiff]: Iterating over directory {}", &root_dir);

    load_hashtab(&root_dir);

    let mut seen_locked = SEEN_FILES.lock().unwrap();

    if let Ok(dir) = std::fs::read_dir(&root_dir) {
        let mut files = vec![];
        for file in dir.flatten() {
            let path: String = file.path().to_string_lossy().to_string();
            if path.ends_with(".qmd") {
                files.push(path);
            }
        }
        files.sort();
        for file in &files {
            let fname_start = match file.rfind("/") {
                Some(e) => e + 1,
                None => 0,
            };
            eprintln!("[qmldiff]: Loading file {}", &file[fname_start..]);
            match load_diff_file(
                Some(root_dir.clone()),
                file,
                &HASHTAB.lock().unwrap(),
                EXTERNAL_LOADER
                    .lock()
                    .unwrap()
                    .map(|e| Box::new(e) as Box<dyn ExternalLoader>),
                Some(&mut seen_locked),
            ) {
                Err(problem) => {
                    eprintln!("[qmldiff]: Failed to load file {}: {:?}", file, problem)
                }
                Ok(mut contents) => {
                    filter_out_non_matching_versions(
                        &mut contents,
                        CURRENT_VERSION.lock().unwrap().clone(),
                        file,
                    );
                    all_changes.extend(contents); // keep raw; slots derived in rebuild
                    loaded_files += 1;
                }
            }
        }
    }

    drop(seen_locked);
    if !all_changes.is_empty() {
        RAW_DIFFS
            .lock()
            .unwrap()
            .push((format!("<files>:{}", root_dir), all_changes));
        *CHANGES_DIRTY.lock().unwrap() = true;
    }
    loaded_files
}

#[no_mangle]
/**
 * # Safety
 * no
 */
pub unsafe extern "C" fn qmldiff_is_modified(file_name: *const c_char) -> bool {
    let file_name: String = CStr::from_ptr(file_name).to_str().unwrap().into();

    if is_building_hashtab() {
        return true;
    }

    ensure_changes();
    CHANGES
        .lock()
        .unwrap()
        .iter()
        .any(|e| match &e.destination {
            ObjectToChange::File(z) | ObjectToChange::FileTokenStream(z) => z == &file_name,
            _ => false,
        })
}

#[no_mangle]
/**
 * # Safety
 * no
 */
pub unsafe extern "C" fn qmldiff_disable_slots_while_processing() {
    *(SLOTS_DISABLED.lock().unwrap()) = true;
}

#[no_mangle]
/**
 * # Safety
 * no
 */
pub unsafe extern "C" fn qmldiff_enable_slots_while_processing() {
    *(SLOTS_DISABLED.lock().unwrap()) = false;
}

#[no_mangle]
/**
 * # Safety
 * no
 */
pub unsafe extern "C" fn qmldiff_process_file(
    file_name: *const c_char,
    raw_contents: *const c_char,
    _contents_size: usize,
) -> *const c_char {
    ensure_changes();
    let mut post_init = POST_INIT.lock().unwrap();
    let are_slots_disabled = SLOTS_DISABLED.lock().unwrap().clone();
    if !*post_init && !are_slots_disabled {
        eprintln!(
            "[qmldiff]: Was asked to process the first slot. Sealing slots, entering postinit..."
        );
        *post_init = true;
        SLOTS
            .lock()
            .unwrap()
            .process_slots(&mut CHANGES.lock().unwrap());
    }
    let file_name: String = CStr::from_ptr(file_name).to_str().unwrap().into();

    if include_if_building_hashtab(&file_name, raw_contents) {
        return std::ptr::null();
    }

    let changes = CHANGES.lock().unwrap();
    // It is modified.
    // Build the tree.
    let contents: String = CStr::from_ptr(raw_contents).to_str().unwrap().into();
    let tree = tokenize_qml(contents, &file_name, None, None);
    eprintln!("[qmldiff]: Processing file {}...", &file_name);
    // Fake slots - when slots are disabled, use the always-empty set of slots in their stead.
    let mut fake_slots = Slots::new();
    let slots = if are_slots_disabled {
        &mut fake_slots
    } else {
        &mut SLOTS.lock().unwrap()
    };
    match find_and_process(&file_name, tree, &changes, slots) {
        Ok((emitted, _count)) => {
            let emitted_string = CString::new(emitted).unwrap();
            let ret = emitted_string.as_ptr();
            std::mem::forget(emitted_string);
            ret
        }
        Err(e) => {
            eprintln!("[qmldiff]: Error while processing file tree: {:?}", e);
            std::ptr::null()
        }
    }
}

#[no_mangle]
pub extern "C" fn qmldiff_start_saving_thread() {
    if std::env::var_os("QMLDIFF_HASHTAB_CREATE").is_some() {
        std::thread::spawn(|| {
            eprintln!("[qmldiff]: Hashtab saver started!");
            loop {
                std::thread::sleep(Duration::from_secs(60));
                if let Some(dist_hashmap_path) = std::env::var_os("QMLDIFF_HASHTAB_CREATE") {
                    let hashtab = match HASHTAB.try_lock() {
                        Ok(ht) => ht,
                        Err(_) => {
                            eprintln!("[qmldiff]: Cannot save hashtab right now. Waiting...");
                            continue;
                        }
                    };
                    let mut to_process_rules = hashtab.clone();
                    if let Some(rules) = HASHTAB_RULES.lock().unwrap().deref() {
                        eprintln!("[qmldiff]: Processing rules.");
                        rules.process(&mut to_process_rules);
                    } else {
                        eprintln!("[qmldiff]: No rules to process.");
                    }
                    let string = serialize_hashtab(
                        &to_process_rules,
                        CURRENT_VERSION.lock().unwrap().clone(),
                    );
                    if let Err(e) = std::fs::write(&dist_hashmap_path, string) {
                        eprintln!(
                            "[qmldiff]: Cannot write to {}: {}",
                            &dist_hashmap_path.to_string_lossy(),
                            e
                        );
                    } else {
                        eprintln!(
                            "[qmldiff]: Hashtab saved to {}",
                            &dist_hashmap_path.to_string_lossy()
                        );
                    }
                }
            }
        });
    }
}
