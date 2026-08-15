//! `mvs solve` — one revision that satisfies several constraints at once.
//!
//! This is the answer to multiverse's one real weakness. Composing versions
//! from *different* revisions gives a closure with two libcs and two opensslls
//! — fine for a leaf CLI, wrong for anything that links. `solve` inverts the
//! question: one revision, one stdenv, internally consistent.
//!
//! Proving *un*satisfiability and saying why is the part no other tool does.
//! mise and asdf cannot, because they do not model compatibility at all.
//!
//! `plan_serving` below asks the sibling question for `mvs lock`: not one
//! revision for everything, but as *few* revisions as a set of already-resolved
//! versions allows. Each pin keeps its version, and pins whose lifetimes
//! overlap share a revision instead of naming one each.

use std::collections::BTreeMap;

use anyhow::{anyhow, Result};
use owo_colors::OwoColorize;
use serde_json::json;

use crate::db::Index;
use crate::output;
use crate::query::Format;
use crate::version;

/// A wanted attribute, and optionally the version prefix it must match.
pub struct Constraint {
    pub attr: String,
    pub version: Option<String>,
}

impl Constraint {
    /// Parse `attr` or `attr@version`.
    pub fn parse(spec: &str) -> Result<Constraint> {
        let (attr, version) = match spec.split_once('@') {
            None => (spec, None),
            Some((attr, ver)) if !ver.is_empty() => (attr, Some(ver.to_string())),
            Some((attr, _)) => {
                return Err(anyhow!(
                    "{attr}@ has no version after the @. Write {attr} for any version, or \
                     {attr}@3.8 for a version."
                ))
            }
        };

        if attr.is_empty() {
            return Err(anyhow!("{spec} has no attribute before the @"));
        }
        Ok(Constraint {
            attr: attr.to_string(),
            version,
        })
    }

    pub fn describe(&self) -> String {
        match &self.version {
            Some(v) => format!("{} {}.x", self.attr, v),
            None => self.attr.clone(),
        }
    }
}

/// Whether `version` satisfies the constraint's prefix.
///
/// Matched component by component rather than by string prefix, which is the
/// difference between `python3@3.1` meaning "3.1.x" and it also matching 3.10,
/// 3.11, 3.12 and 3.13. A GLOB in SQL cannot express this, so the filter is
/// applied in Rust after the query.
pub fn matches(version: &str, prefix: &str) -> bool {
    let mut wanted = version::components(prefix);
    let mut have = version::components(version);

    loop {
        match (wanted.next(), have.next()) {
            // The constraint ran out: everything it asked for matched.
            (None, _) => return true,
            // The version ran out first, so it is shorter than the constraint.
            (Some(_), None) => return false,
            (Some(w), Some(h)) if w != h => return false,
            _ => {}
        }
    }
}

/// A closed range of offsets in which one constraint holds throughout.
pub type Span = (i64, i64);

/// Every stretch of revisions in which one constraint is satisfied, merged and
/// in offset order.
pub fn spans_for(index: &Index, constraint: &Constraint) -> Result<Vec<Span>> {
    let runs = index.runs_of(&constraint.attr)?;
    if runs.is_empty() {
        return Err(anyhow!(
            "{} is not in the index, so nothing can satisfy it.",
            constraint.attr
        ));
    }

    let mut spans: Vec<Span> = runs
        .iter()
        .filter(|run| match &constraint.version {
            None => true,
            Some(prefix) => matches(&run.version, prefix),
        })
        .map(|run| (run.first, run.last))
        .collect();
    spans.sort();

    // Adjacent runs of *different* versions both satisfying the constraint —
    // 3.8.9 followed by 3.8.10 — are one stretch as far as the caller is
    // concerned, and merging them keeps the intersection below linear.
    let mut merged: Vec<Span> = Vec::new();
    for (first, last) in spans {
        match merged.last_mut() {
            Some(prev) if first <= prev.1 + 1 => prev.1 = prev.1.max(last),
            _ => merged.push((first, last)),
        }
    }
    Ok(merged)
}

/// Intersect two sets of spans, both in offset order.
fn intersect(a: &[Span], b: &[Span]) -> Vec<Span> {
    let (mut i, mut j) = (0, 0);
    let mut out = Vec::new();

    while i < a.len() && j < b.len() {
        let first = a[i].0.max(b[j].0);
        let last = a[i].1.min(b[j].1);
        if first <= last {
            out.push((first, last));
        }

        // Advance whichever span ends first: the other may still overlap the
        // next one along.
        if a[i].1 < b[j].1 {
            i += 1;
        } else {
            j += 1;
        }
    }
    out
}

/// The newest offset in `spans` that a pin can name: newest first, walking
/// back over any revision that has no narHash and so cannot be fetched.
pub fn newest_pinnable(index: &Index, spans: &[Span]) -> Result<i64> {
    for (first, last) in spans.iter().rev() {
        if let Some(off) = index.newest_materialisable_in(*first, *last)? {
            return Ok(off);
        }
    }

    Err(anyhow!(
        "every revision satisfying this has no narHash yet, so none of them can be \
         fetched. Run tools/add-narhashes.sh, or wait for the next index build."
    ))
}

fn count(spans: &[Span]) -> i64 {
    spans.iter().map(|(first, last)| last - first + 1).sum()
}

/// Whether an offset falls inside any of these spans.
pub fn within(spans: &[Span], off: i64) -> bool {
    spans
        .iter()
        .any(|(first, last)| *first <= off && off <= *last)
}

/// One pin awaiting a serving revision: its version is already decided, and
/// these are the places that version was seen.
pub struct ServeTarget {
    /// The attribute, which is what the plan keys its answer by.
    pub key: String,
    /// Every span carrying the resolved version.
    pub spans: Vec<Span>,
    /// The newest materialisable offset in each span, the only offsets a plan
    /// ever picks. Never empty: the offset that decided the version is one.
    pub candidates: Vec<i64>,
}

/// Choose a serving revision for every target, using as few distinct
/// revisions as the spans allow.
///
/// `fixed` are revisions already paid for, meaning the pins the caller is not
/// touching. Any target one of them can serve takes the newest such and costs
/// nothing. The rest is greedy set cover, the same plan `resolvePins` makes
/// on the Nix side: repeatedly take the offset serving the most remaining
/// targets, newest offset among equals. Only targets move; nothing here ever
/// proposes moving a fixed revision.
pub fn plan_serving(targets: &[ServeTarget], fixed: &[i64]) -> BTreeMap<String, i64> {
    let mut assigned = BTreeMap::new();

    let mut remaining: Vec<&ServeTarget> = targets
        .iter()
        .filter(
            |t| match fixed.iter().filter(|&&off| within(&t.spans, off)).max() {
                Some(&off) => {
                    assigned.insert(t.key.clone(), off);
                    false
                }
                None => true,
            },
        )
        .collect();

    while !remaining.is_empty() {
        // The best offset always serves at least the target it is a candidate
        // of, because candidates lie inside their own spans, so every round
        // shrinks `remaining` and the loop terminates.
        let best = remaining
            .iter()
            .flat_map(|t| t.candidates.iter().copied())
            .max_by_key(|&off| {
                (
                    remaining.iter().filter(|t| within(&t.spans, off)).count(),
                    off,
                )
            })
            .expect("every target carries at least one candidate");
        remaining.retain(|t| {
            if within(&t.spans, best) {
                assigned.insert(t.key.clone(), best);
                false
            } else {
                true
            }
        });
    }
    assigned
}

pub fn solve(index: &Index, specs: &[String], format: Format) -> Result<()> {
    let constraints = specs
        .iter()
        .map(|s| Constraint::parse(s))
        .collect::<Result<Vec<_>>>()?;
    if constraints.is_empty() {
        return Err(anyhow!("give at least one constraint, e.g. python3@3.8"));
    }

    let mut each: Vec<(&Constraint, Vec<Span>)> = Vec::new();
    for constraint in &constraints {
        each.push((constraint, spans_for(index, constraint)?));
    }

    // The whole solve: intersect the constraints' ranges. Ordering by how
    // narrow each one is empties an unsatisfiable intersection sooner, and
    // costs one sort.
    let mut ordered: Vec<usize> = (0..each.len()).collect();
    ordered.sort_by_key(|&i| count(&each[i].1));

    let mut solution = each[ordered[0]].1.clone();
    for &i in &ordered[1..] {
        solution = intersect(&solution, &each[i].1);
    }

    if solution.is_empty() {
        return unsatisfiable(index, &each, format);
    }

    let newest = index.revision(solution.last().unwrap().1)?;
    let oldest = index.revision(solution[0].0)?;
    let total = count(&solution);

    if format == Format::Json {
        let versions = versions_at(index, &constraints, newest.off)?;
        return output::print_json(json!({
            "satisfiable": true,
            "revisions": total,
            "first": oldest,
            "last": newest,
            "versions": versions,
            "spans": solution.iter().map(|(f, l)| json!({"first": f, "last": l})).collect::<Vec<_>>(),
        }));
    }

    anstream::println!(
        "{} · {} .. {}",
        output::plural(total as usize, "revision"),
        oldest.date,
        newest.date
    );
    anstream::println!(
        "newest: {} ({})",
        newest.rev[..12].to_string().style(output::current()),
        newest.date
    );

    // What the newest solution actually provides, so the caller can see the
    // resolved versions rather than the constraints they typed.
    let mut table = output::Table::new(&["ATTR", "VERSION"]);
    for (attr, version) in versions_at(index, &constraints, newest.off)? {
        table.row(vec![
            output::Cell::new(attr, output::plain()),
            output::Cell::new(version, output::current()),
        ]);
    }
    anstream::println!();
    table.print();

    anstream::println!(
        "\n  inputs.nixpkgs.url = \"github:NixOS/nixpkgs/{}\";",
        newest.rev
    );
    Ok(())
}

/// The versions the constraints resolve to at one revision.
fn versions_at(
    index: &Index,
    constraints: &[Constraint],
    off: i64,
) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    for constraint in constraints {
        let version = index
            .runs_of(&constraint.attr)?
            .into_iter()
            .find(|r| r.first <= off && off <= r.last)
            .map(|r| r.version)
            .unwrap_or_else(|| "?".to_string());
        out.push((constraint.attr.clone(), version));
    }
    Ok(out)
}

/// Say *why* nothing satisfies the constraints.
///
/// The useful answer is rarely "no": it is which two constraints never
/// overlapped, and on which side of each other they sit. So each constraint is
/// reported with its own range, and the first disjoint pair is named.
fn unsatisfiable(index: &Index, each: &[(&Constraint, Vec<Span>)], format: Format) -> Result<()> {
    let mut reported = Vec::new();
    for (constraint, spans) in each {
        let ends = match (spans.first(), spans.last()) {
            (Some(first), Some(last)) => Some((index.revision(first.0)?, index.revision(last.1)?)),
            _ => None,
        };
        reported.push((constraint, spans, ends));
    }

    // The first pair with no overlap at all. With more than two constraints
    // this is the one to name: an intersection can be empty while every
    // constraint still overlaps some other.
    let mut culprits = None;
    for i in 0..each.len() {
        for j in i + 1..each.len() {
            if intersect(&each[i].1, &each[j].1).is_empty() {
                culprits = Some((i, j));
                break;
            }
        }
    }

    if format == Format::Json {
        let constraints = reported
            .iter()
            .map(|(c, spans, ends)| {
                json!({
                    "attr": c.attr,
                    "version": c.version,
                    "revisions": count(spans),
                    "first": ends.as_ref().map(|(f, _)| f.date.clone()),
                    "last": ends.as_ref().map(|(_, l)| l.date.clone()),
                })
            })
            .collect::<Vec<_>>();
        output::print_json(json!({
            "satisfiable": false,
            "constraints": constraints,
            "disjoint": culprits.map(|(i, j)| json!([each[i].0.describe(), each[j].0.describe()])),
        }))?;

        // Nothing was found, and a script should be able to branch on that
        // without parsing either the message or the JSON.
        std::process::exit(1);
    }

    let subject = match each.len() {
        1 => each[0].0.describe(),
        2 => "both".to_string(),
        _ => "all of these".to_string(),
    };
    anstream::println!(
        "{}",
        format!("no revision ever had {subject}").style(output::ended())
    );

    // Both ends of each constraint's range, rather than whichever end seems to
    // be the reason: with three constraints the "too old" and "too new" framing
    // depends on which pair you are looking at, and the dates say it anyway.
    let mut table = output::Table::new(&["WANTED", "FROM", "TO", "REVS"]);
    for (constraint, spans, ends) in &reported {
        match ends {
            None => table.row(vec![
                output::Cell::new(constraint.describe(), output::plain()),
                output::Cell::new("never existed", output::ended()),
                output::Cell::new("", output::plain()),
                output::Cell::new("0", output::muted()),
            ]),
            Some((first, last)) => {
                let current = last.off >= index.covered_tip();
                table.row(vec![
                    output::Cell::new(constraint.describe(), output::plain()),
                    output::Cell::new(&first.date, output::plain()),
                    output::Cell::new(
                        if current { "current" } else { &last.date },
                        if current {
                            output::current()
                        } else {
                            output::ended()
                        },
                    ),
                    output::Cell::new(count(spans).to_string(), output::muted()),
                ]);
            }
        }
    }
    table.print();

    if let Some((i, j)) = culprits {
        anstream::println!(
            "{}",
            format!(
                "  {} and {} never overlapped",
                each[i].0.describe(),
                each[j].0.describe()
            )
            .style(output::muted())
        );
    }

    std::process::exit(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Constraint parsing: attr alone, attr@version, and the two malformed
    /// shapes that would otherwise turn into a silent match-everything.
    #[test]
    fn parses_constraints() {
        let c = Constraint::parse("python3@3.8").unwrap();
        assert_eq!(c.attr, "python3");
        assert_eq!(c.version.as_deref(), Some("3.8"));

        let c = Constraint::parse("ripgrep").unwrap();
        assert_eq!(c.version, None);

        assert!(Constraint::parse("python3@").is_err());
        assert!(Constraint::parse("@3.8").is_err());
    }

    /// Prefix matching by component, which is what keeps `3.1` from meaning
    /// 3.10 through 3.13 — the bug a string prefix or a SQL GLOB would have.
    #[test]
    fn matches_by_component_not_by_string() {
        assert!(matches("3.8.9", "3.8"));
        assert!(matches("3.8", "3.8"));
        assert!(matches("14.1.1", "14"));
        assert!(matches("1.12-nightly", "1.12"));

        assert!(!matches("3.10.2", "3.1"));
        assert!(!matches("3.8", "3.8.9"));
        assert!(!matches("140.0", "14"));
    }

    /// Span intersection, the core of solve: overlapping, touching-but-not-
    /// overlapping, and one span spanning several of the other.
    #[test]
    fn intersects_spans() {
        assert_eq!(intersect(&[(0, 10)], &[(5, 20)]), [(5, 10)]);
        assert_eq!(intersect(&[(0, 4)], &[(5, 20)]), []);
        assert_eq!(
            intersect(&[(0, 100)], &[(5, 10), (20, 30)]),
            [(5, 10), (20, 30)]
        );
        assert_eq!(intersect(&[], &[(5, 10)]), []);
    }

    fn target(key: &str, spans: &[Span], candidates: &[i64]) -> ServeTarget {
        ServeTarget {
            key: key.to_string(),
            spans: spans.to_vec(),
            candidates: candidates.to_vec(),
        }
    }

    /// Overlapping targets share one revision; disjoint ones get one each,
    /// resolved newest-first so the lone newest pin lands exactly where the
    /// old per-pin rule put it.
    #[test]
    fn plans_shared_revisions() {
        let plan = plan_serving(
            &[target("a", &[(0, 10)], &[10]), target("b", &[(5, 8)], &[8])],
            &[],
        );
        assert_eq!(plan["a"], 8);
        assert_eq!(plan["b"], 8);

        let plan = plan_serving(
            &[target("a", &[(0, 4)], &[4]), target("b", &[(6, 9)], &[9])],
            &[],
        );
        assert_eq!(plan["a"], 4);
        assert_eq!(plan["b"], 9);
    }

    /// Coverage beats freshness, freshness breaks ties: the point serving both
    /// targets wins over the newer point serving one.
    #[test]
    fn prefers_coverage_then_newest() {
        let plan = plan_serving(
            &[
                target("a", &[(0, 10)], &[10]),
                target("b", &[(0, 12)], &[12]),
            ],
            &[],
        );
        assert_eq!(plan["a"], 10);
        assert_eq!(plan["b"], 10);
    }

    /// A pierce point can sit at another target's span end: gaps in one
    /// target's lifetime do not stop it sharing where the runs overlap.
    #[test]
    fn plans_across_gaps() {
        let plan = plan_serving(
            &[
                target("a", &[(0, 2), (8, 9)], &[2, 9]),
                target("b", &[(0, 3)], &[3]),
            ],
            &[],
        );
        assert_eq!(plan["a"], 2);
        assert_eq!(plan["b"], 2);
    }

    /// Fixed revisions are free: a target one can serve takes the newest such
    /// fixed revision instead of anything from the cover, and a fixed
    /// revision outside every span changes nothing.
    #[test]
    fn reuses_fixed_revisions() {
        let plan = plan_serving(&[target("a", &[(0, 10)], &[10])], &[3, 7]);
        assert_eq!(plan["a"], 7);

        let plan = plan_serving(&[target("a", &[(0, 10)], &[10])], &[20]);
        assert_eq!(plan["a"], 10);
    }
}
