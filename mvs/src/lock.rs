//! `mvs lock` — per-package pins.
//!
//! Each pin names one revision, and `mvs lock update <attr>` moves **only** that
//! entry. Every other pin stays exactly where it was, which is the whole point:
//! a single flake input moves everything at once, and that is why people end up
//! not updating at all.
//!
//! Within that contract, pins being added or updated resolve *together*, onto
//! as few revisions as their versions allow. It is the lock-side twin of the
//! Nix API's `resolvePins`. Sharing never changes a resolved version, only
//! which revision serves it, and untouched pins are read as candidates to
//! share with, never rewritten.
//!
//! The two-step workflow this implies is honest rather than accidental. A pin
//! can never point past what the index knows, because materialising a revision
//! needs its narHash and `mvs` only has the ones in its baked database:
//!
//! ```console
//! $ nix flake update multiverse    # learn about newer revisions
//! $ mvs lock update helix           # move this one package
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use owo_colors::OwoColorize;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::date;
use crate::db::Index;
use crate::output::{self, Cell, Table};
use crate::query::Format;
use crate::solve::{newest_pinnable, plan_serving, spans_for, Constraint, ServeTarget, Span};
use crate::version;

/// The lock file's name, and the one `multiverse.lib.readLock` expects.
pub const LOCK_FILE: &str = "multiverse.lock";

/// Schema version of the lock file. Bumped only for a change that an older
/// `mvs` could misread; a new optional field does not need one.
pub const LOCK_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
pub struct Lock {
    pub version: u32,
    /// Sorted by attribute, so a pin added today produces a one-line diff
    /// wherever it lands alphabetically rather than a reordering.
    pub pins: BTreeMap<String, Pin>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Pin {
    pub rev: String,
    pub label: String,
    pub version: String,
    pub date: String,

    /// The version prefix the pin was added with, if any. `mvs lock update`
    /// stays inside it — a pin added as `python3@3.8` is a decision to be on
    /// 3.8, and update must not silently walk it to 3.14.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub constraint: Option<String>,
}

impl Lock {
    fn empty() -> Lock {
        Lock {
            version: LOCK_VERSION,
            pins: BTreeMap::new(),
        }
    }

    /// Read the lock file, or an empty lock if there is none yet.
    pub fn read(path: &Path) -> Result<Lock> {
        if !path.exists() {
            return Ok(Lock::empty());
        }

        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let lock: Lock =
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;

        if lock.version != LOCK_VERSION {
            return Err(anyhow!(
                "{} is version {}, and this mvs understands version {LOCK_VERSION}. \
                 Update multiverse.",
                path.display(),
                lock.version
            ));
        }
        Ok(lock)
    }

    /// Write the lock file. Pretty-printed with a trailing newline: it is a
    /// committed file that shows up in review.
    pub fn write(&self, path: &Path) -> Result<()> {
        let mut text = serde_json::to_string_pretty(self)?;
        text.push('\n');
        std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))
    }
}

/// Where the lock file lives: `--file` if given, otherwise `multiverse.lock` in
/// the working directory. No search up the tree — a pin file that is picked up
/// from a parent directory is a pin file you did not know you were editing.
pub fn lock_path(explicit: Option<&Path>) -> PathBuf {
    explicit
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from(LOCK_FILE))
}

/// Resolve a constraint to the revision a pin should name: the newest indexed
/// revision that satisfies it *and* can be materialised.
///
/// This is the reference `status` measures against. `add` and `update` resolve
/// the same *version*, decided by the newest satisfying revision, but choose
/// the serving revision through `plan_pins`, which may share one across pins.
fn pin_for(index: &Index, constraint: &Constraint) -> Result<Pin> {
    let spans = spans_for(index, constraint)?;
    if spans.is_empty() {
        return Err(anyhow!("no revision ever had {}", constraint.describe()));
    }

    let off = newest_pinnable(index, &spans)?;
    let revision = index.revision(off)?;
    let version = index
        .runs_of(&constraint.attr)?
        .into_iter()
        .find(|r| r.first <= off && off <= r.last)
        .map(|r| r.version)
        .ok_or_else(|| anyhow!("{} is not in {}", constraint.attr, revision.label))?;

    Ok(Pin {
        rev: revision.rev,
        label: revision.label,
        version,
        date: revision.date,
        constraint: constraint.version.clone(),
    })
}

/// A pin whose version is decided but whose serving revision is not yet: the
/// version `pin_for` would give it, every span carrying that version, and the
/// newest materialisable offset in each. Which revision *does* serve it is the
/// plan's call.
struct Target {
    constraint: Constraint,
    version: String,
    spans: Vec<Span>,
    candidates: Vec<i64>,
}

fn target_for(index: &Index, constraint: Constraint) -> Result<Target> {
    let spans = spans_for(index, &constraint)?;
    if spans.is_empty() {
        return Err(anyhow!("no revision ever had {}", constraint.describe()));
    }

    let off = newest_pinnable(index, &spans)?;
    let runs = index.runs_of(&constraint.attr)?;
    let version = runs
        .iter()
        .find(|r| r.first <= off && off <= r.last)
        .map(|r| r.version.clone())
        .ok_or_else(|| {
            anyhow!(
                "{} is not in the index it just resolved from",
                constraint.attr
            )
        })?;

    let vspans: Vec<Span> = runs
        .iter()
        .filter(|r| r.version == version)
        .map(|r| (r.first, r.last))
        .collect();
    let mut candidates = Vec::new();
    for (first, last) in &vspans {
        if let Some(c) = index.newest_materialisable_in(*first, *last)? {
            candidates.push(c);
        }
    }
    // `off` is materialisable and lies in one of these spans, so this cannot
    // trigger unless the database changed under us mid-command.
    if candidates.is_empty() {
        return Err(anyhow!(
            "no revision carrying {} {version} can be materialised",
            constraint.attr
        ));
    }

    Ok(Target {
        constraint,
        version,
        spans: vspans,
        candidates,
    })
}

/// Serving revisions for several targets at once, sharing wherever the
/// versions' lifetimes allow, including with `untouched`, the pins this
/// command is not moving: their revisions are already paid for, so a target
/// one of them satisfies lands there for free. Untouched pins are only ever
/// read; the plan cannot move them.
fn plan_pins(
    index: &Index,
    targets: Vec<Target>,
    untouched: &BTreeMap<String, Pin>,
) -> Result<BTreeMap<String, Pin>> {
    let mut fixed = Vec::new();
    for pin in untouched.values() {
        // A revision the database does not know, such as a lock written
        // against a newer index, cannot be shared with. That is not an error
        // here; the pin itself is untouched either way.
        if let Some(revision) = index.revision_by_prefix(&pin.rev)? {
            fixed.push(revision.off);
        }
    }

    let serve: Vec<ServeTarget> = targets
        .iter()
        .map(|t| ServeTarget {
            key: t.constraint.attr.clone(),
            spans: t.spans.clone(),
            candidates: t.candidates.clone(),
        })
        .collect();
    let plan = plan_serving(&serve, &fixed);

    let mut pins = BTreeMap::new();
    for target in targets {
        let revision = index.revision(plan[&target.constraint.attr])?;
        pins.insert(
            target.constraint.attr.clone(),
            Pin {
                rev: revision.rev,
                label: revision.label,
                version: target.version,
                date: revision.date,
                constraint: target.constraint.version,
            },
        );
    }
    Ok(pins)
}

/// `lock: N pins on M revisions`, the consolidation at a glance, printed
/// after anything that wrote the file. Skipped for a single pin, where it
/// could only state the obvious.
fn print_footprint(lock: &Lock) {
    if lock.pins.len() < 2 {
        return;
    }
    let revisions: BTreeSet<&str> = lock.pins.values().map(|p| p.rev.as_str()).collect();
    anstream::println!(
        "{}",
        format!(
            "lock: {} on {}",
            output::plural(lock.pins.len(), "pin"),
            output::plural(revisions.len(), "revision")
        )
        .style(output::muted())
    );
}

/// `mvs lock add <attr>[@ver]...`
///
/// Every spec resolves to its own version exactly as a lone `add` would; the
/// set then shares serving revisions wherever those versions overlap, with
/// each other and with the pins already in the file.
pub fn add(index: &Index, path: &Path, specs: &[String], format: Format) -> Result<()> {
    let mut seen = BTreeSet::new();
    let mut constraints = Vec::new();
    for spec in specs {
        let constraint = Constraint::parse(spec)?;
        if !seen.insert(constraint.attr.clone()) {
            return Err(anyhow!("{} is given more than once", constraint.attr));
        }
        constraints.push(constraint);
    }

    let mut lock = Lock::read(path)?;

    // A pin being re-added is being re-decided, so only the others hold still
    // and offer their revisions.
    let untouched: BTreeMap<String, Pin> = lock
        .pins
        .iter()
        .filter(|(attr, _)| !seen.contains(*attr))
        .map(|(attr, pin)| (attr.clone(), pin.clone()))
        .collect();

    let targets = constraints
        .into_iter()
        .map(|c| target_for(index, c))
        .collect::<Result<Vec<_>>>()?;
    let planned = plan_pins(index, targets, &untouched)?;

    let mut added = Vec::new();
    for (attr, pin) in planned {
        let previous = lock.pins.insert(attr.clone(), pin.clone());
        added.push((attr, pin, previous.is_some()));
    }
    lock.write(path)?;

    if format == Format::Json {
        let pins: BTreeMap<&String, &Pin> =
            added.iter().map(|(attr, pin, _)| (attr, pin)).collect();
        return output::print_json(json!({ "pins": pins }));
    }

    for (attr, pin, repinned) in &added {
        let sharing: Vec<&str> = lock
            .pins
            .iter()
            .filter(|(other, p)| *other != attr && p.rev == pin.rev)
            .map(|(other, _)| other.as_str())
            .collect();
        let verb = if *repinned { "repinned" } else { "pinned" };
        anstream::println!(
            "{verb} {attr} {} at {}{}",
            pin.version.style(output::current()),
            pin.label,
            if sharing.is_empty() {
                String::new()
            } else {
                format!(" (with {})", sharing.join(", "))
            }
        );
    }
    print_footprint(&lock);
    Ok(())
}

/// `mvs lock rm <attr>`
pub fn remove(path: &Path, attr: &str, format: Format) -> Result<()> {
    let mut lock = Lock::read(path)?;
    let removed = lock
        .pins
        .remove(attr)
        .ok_or_else(|| anyhow!("{attr} is not pinned in {}", path.display()))?;
    lock.write(path)?;

    if format == Format::Json {
        return output::print_json(json!({ "attr": attr, "removed": removed }));
    }
    anstream::println!(
        "unpinned {attr} (was {} at {})",
        removed.version,
        removed.label
    );
    Ok(())
}

/// `mvs lock update [<attr>]`: move one pin, or every pin with `--all`.
///
/// Only the named entries can move; that is the whole difference from a flake
/// input. They are *replanned* together: each resolves to the newest version
/// its constraint allows, exactly as before, and the serving revisions are
/// shared among the targets and with the pins that are not moving. So
/// `update <attr>` consolidates onto a revision the lock already pays for
/// when one carries the right version, and `update --all` regroups the whole
/// file onto as few revisions as the versions allow.
pub fn update(
    index: &Index,
    path: &Path,
    attr: Option<&str>,
    all: bool,
    format: Format,
) -> Result<()> {
    let mut lock = Lock::read(path)?;
    if lock.pins.is_empty() {
        return Err(anyhow!("{} has no pins", path.display()));
    }

    let target_names: Vec<String> = match (attr, all) {
        (Some(attr), _) => {
            if !lock.pins.contains_key(attr) {
                return Err(anyhow!("{attr} is not pinned in {}", path.display()));
            }
            vec![attr.to_string()]
        }
        (None, true) => lock.pins.keys().cloned().collect(),
        (None, false) => {
            return Err(anyhow!(
                "name a package to update, or pass --all to move every pin"
            ))
        }
    };

    let untouched: BTreeMap<String, Pin> = lock
        .pins
        .iter()
        .filter(|(attr, _)| !target_names.contains(attr))
        .map(|(attr, pin)| (attr.clone(), pin.clone()))
        .collect();

    let targets = target_names
        .iter()
        .map(|attr| {
            target_for(
                index,
                Constraint {
                    attr: attr.clone(),
                    version: lock.pins[attr].constraint.clone(),
                },
            )
        })
        .collect::<Result<Vec<_>>>()?;
    let planned = plan_pins(index, targets, &untouched)?;

    let mut moved = Vec::new();
    for (attr, new) in planned {
        let old = lock.pins[&attr].clone();
        if new.rev != old.rev {
            moved.push(json!({
                "attr": attr,
                "from": { "version": old.version, "label": old.label },
                "to": { "version": new.version, "label": new.label },
            }));
            lock.pins.insert(attr, new);
        }
    }

    if !moved.is_empty() {
        lock.write(path)?;
    }

    if format == Format::Json {
        return output::print_json(json!({ "moved": moved }));
    }

    if moved.is_empty() {
        anstream::println!("every pin is already where the plan puts it");
        return Ok(());
    }
    for entry in &moved {
        let from_version = entry["from"]["version"].as_str().unwrap();
        let to_version = entry["to"]["version"].as_str().unwrap();
        if from_version == to_version {
            // The version held and only the serving revision moved, so say
            // where it went from and to, or the line would read as a no-op.
            anstream::println!(
                "{}: {} · {} → {}",
                entry["attr"].as_str().unwrap(),
                to_version.style(output::current()),
                entry["from"]["label"].as_str().unwrap(),
                entry["to"]["label"].as_str().unwrap()
            );
        } else {
            anstream::println!(
                "{}: {} → {}  ({})",
                entry["attr"].as_str().unwrap(),
                from_version,
                to_version.style(output::current()),
                entry["to"]["label"].as_str().unwrap()
            );
        }
    }
    print_footprint(&lock);
    Ok(())
}

/// `mvs lock list`
pub fn list(path: &Path, format: Format) -> Result<()> {
    let lock = Lock::read(path)?;

    if format == Format::Json {
        return output::print_json(serde_json::to_value(&lock)?);
    }

    if lock.pins.is_empty() {
        anstream::println!("{} has no pins", path.display());
        return Ok(());
    }

    let mut table = Table::new(&["ATTR", "VERSION", "DATE", "REVISION"]);
    for (attr, pin) in &lock.pins {
        table.row(vec![
            Cell::new(attr, output::plain()),
            Cell::new(&pin.version, output::plain()),
            Cell::new(&pin.date, output::muted()),
            Cell::new(&pin.rev[..12], output::muted()),
        ]);
    }
    table.print();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_lock(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mvs-lock-test-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(LOCK_FILE)
    }

    /// The file format, which is a committed artifact and so has to survive a
    /// round trip byte for byte: reads back what it wrote, keeps pins sorted by
    /// attribute, and omits `constraint` entirely when there is none.
    #[test]
    fn round_trips_the_lock_file() {
        let path = temp_lock("round-trip");
        let mut lock = Lock::empty();
        lock.pins.insert(
            "ripgrep".to_string(),
            Pin {
                rev: "7c6e3666e2040fb64d43b209b84f65898ea3095d".to_string(),
                label: "2023-11-29-7c6e3666e204".to_string(),
                version: "13.0.0".to_string(),
                date: "2023-11-29".to_string(),
                constraint: Some("13".to_string()),
            },
        );
        lock.pins.insert(
            "helix".to_string(),
            Pin {
                rev: "2fcb964de67fcf60b43471c55d5d99e61a9ccb5a".to_string(),
                label: "2026-08-10-2fcb964de67f".to_string(),
                version: "25.07.1".to_string(),
                date: "2026-08-10".to_string(),
                constraint: None,
            },
        );
        lock.write(&path).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.ends_with('\n'), "a committed file ends with a newline");
        assert!(!text.contains("\"constraint\": null"), "{text}");
        // helix was inserted second and must still come first on disk.
        assert!(text.find("helix") < text.find("ripgrep"));

        let read = Lock::read(&path).unwrap();
        assert_eq!(read.pins.len(), 2);
        assert_eq!(read.pins["ripgrep"].constraint.as_deref(), Some("13"));
        assert_eq!(read.pins["helix"].constraint, None);
        std::fs::remove_file(&path).ok();
    }

    /// Absent and unreadable lock files. A missing one is an empty lock, since
    /// `mvs lock add` has to work in a directory that has none; a future format
    /// version is an error rather than a guess.
    #[test]
    fn handles_missing_and_future_files() {
        let path = temp_lock("missing");
        assert!(Lock::read(&path).unwrap().pins.is_empty());

        std::fs::write(&path, r#"{"version": 99, "pins": {}}"#).unwrap();
        let err = match Lock::read(&path) {
            Err(err) => err.to_string(),
            Ok(_) => panic!("a future lock version was accepted"),
        };
        assert!(err.contains("version 99"), "{err}");
        std::fs::remove_file(&path).ok();
    }
}

/// `mvs lock status` — how far behind each pin has fallen.
///
/// This is where the history index earns its place: "3 versions and 47 days
/// behind" with nothing fetched and no clock consulted. Both numbers are
/// measured against the newest revision the index knows, not against today, so
/// the answer is reproducible and moves only when the index does.
pub fn status(index: &Index, path: &Path, format: Format) -> Result<()> {
    let lock = Lock::read(path)?;
    if lock.pins.is_empty() {
        if format == Format::Json {
            return output::print_json(json!({ "pins": [] }));
        }
        anstream::println!("{} has no pins", path.display());
        return Ok(());
    }

    let mut rows = Vec::new();
    for (attr, pin) in &lock.pins {
        let constraint = Constraint {
            attr: attr.clone(),
            version: pin.constraint.clone(),
        };
        let newest = pin_for(index, &constraint)?;

        // Versions behind counts what is actually reachable under the pin's own
        // constraint: a pin held at python3@3.8 is not "6 versions behind" 3.14,
        // it is exactly where it was asked to be.
        let mut newer: Vec<String> = index
            .runs_of(attr)?
            .into_iter()
            .filter(|r| match &pin.constraint {
                Some(prefix) => crate::solve::matches(&r.version, prefix),
                None => true,
            })
            .map(|r| r.version)
            .filter(|v| version::compare(v, &pin.version) == std::cmp::Ordering::Greater)
            .collect();
        newer.sort_by(|a, b| version::compare(a, b));
        newer.dedup();

        rows.push(json!({
            "attr": attr,
            "version": pin.version,
            "date": pin.date,
            "latest": newest.version,
            "latest_label": newest.label,
            "versions_behind": newer.len(),
            "days_behind": date::days_between(&pin.date, &newest.date),
        }));
    }

    if format == Format::Json {
        return output::print_json(json!({ "pins": rows }));
    }

    let mut table = Table::new(&["ATTR", "PINNED", "LATEST", "BEHIND"]);
    for row in &rows {
        let behind = row["versions_behind"].as_u64().unwrap();
        let days = row["days_behind"].as_i64().unwrap();
        table.row(vec![
            Cell::new(row["attr"].as_str().unwrap(), output::plain()),
            Cell::new(row["version"].as_str().unwrap(), output::plain()),
            Cell::new(
                row["latest"].as_str().unwrap(),
                if behind == 0 {
                    output::current()
                } else {
                    output::ended()
                },
            ),
            Cell::new(
                if behind == 0 {
                    "current".to_string()
                } else {
                    format!(
                        "{}, {}",
                        output::plural(behind as usize, "version"),
                        output::plural(days.max(0) as usize, "day")
                    )
                },
                if behind == 0 {
                    output::current()
                } else {
                    output::muted()
                },
            ),
        ]);
    }
    table.print();
    Ok(())
}
