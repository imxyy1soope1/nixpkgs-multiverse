# Design

Nixpkgs history *already is* the multiverse. Every version that ever existed is
already built, already cached, already reachable, it was just addressed by
commit hash instead of by version number, which is exactly backwards from how
anyone thinks about it.

This project does not build old packages. It builds an address book.

## Why

The problem starts the first time a version bump breaks something. The usual
fix is a second `nixpkgs` input pinned to the commit before the bump, and it
works. The trouble is what it costs, because every pin is a whole extra
nixpkgs in the file:

```nix
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    nixpkgs-for-vscode.url = "github:NixOS/nixpkgs/8b8c811c7c25";
    nixpkgs-for-ripgrep.url = "github:NixOS/nixpkgs/967d40bec14b";
    # ...and one more every time something else breaks
  };
}
```

Each of those is a separate input to update, a separate line to explain, and a
separate thing to forget the reason for. The file grows a pin per incident and
never loses one.

It is also slow, for a reason that is not obvious: **flake inputs are fetched
eagerly, whether or not anything references them** as of Nix 2.34.

## How is this possible?

Nothing here is a trick.

Every package in the store describes its dependencies exactly, by hash. The
hash uniquely identifies the set of inputs a build actually used, so two
builds of the same package from different revisions, different sources,
different dependency trees, different compilers, etc., are simply two different
paths. They coexist. There is no global version to conflict over, no
"installed" version to displace, and nothing to resolve.

The second half is that the work is already done. We purposefully only leverage
bumps to Nix's unstable channel. Hydra built these revisions
when they were current and pushed them to [cache.nixos.org](https://cache.nixos.org), where they remain. Every version this index names is a cache hit.

So the missing piece was never building or storing. It was *addressing*: a way
to say "python3 3.6.2" instead of "nixpkgs at 967d40bec14b", and to say it
without paying for the other revisions you did not ask about — 1,537 of
them, as of 2026-08-10.

## Lazy trees

`flake.nix` has `inputs = { }` on purpose, and that is the whole design in one
line. Nothing this flake can reach is an input, so nothing is eager.

Revisions are fetched with `builtins.fetchTree`, pinned by `narHash`, at the
moment a derivation is forced and not before:

```nix
builtins.fetchTree {
  type = "github";
  owner = "NixOS";
  repo = "nixpkgs";
  rev = r.rev;
  inherit (r) narHash;
}
```

Everything upstream of that call: enumerating versions, resolving a date to a
revision, reading a lifetime is a Nix evaluation over JSON that ships with the
flake. It fetches nothing. A revision becomes a real tree only when you force
a derivation out of it.

Cost is therefore per *revision touched*, not per package. Revisions are
memoised, so three packages from one revision cost what one package costs;
three packages from three revisions cost three fetches. Each revision actually
used is a one-time ~378 MB which is the size of the Nixpkgs tree.

There is a second reason inputs could not work even if they were lazy: nixpkgs
had no `flake.nix` before 20.03. Revisions older than that cannot be flake
inputs at all, and roughly the first third of this index predates it.

## The index

Four files, none of which grows with the number of revisions in the way the
obvious encoding would. Every count below is a measurement taken on
2026-08-10 and left there; the index grows hourly, and the [status block in
the README](../README.md#status) is what carries the current figures.

`revisions.json` is the spine: 1,538 nixos-unstable channel bumps from
2013-10-31 to 2026-08-10, each with its commit, date, channel name and
`narHash`. Everything else refers to a revision by its **offset** into this
array, which is why the other files stay small.

`index/versions.json` maps each (attribute, version) to the single newest
revision that shipped it:

```json
{
  "revisionCount": 1540,
  "attrs": {
    "hello": { "2.8": 0, "2.12.2": 1494, "2.12.3": null }
  }
}
```

`null` is not "unknown" — it is the newest revision the file covers,
`revisionCount - 1`, and it is how the file says a version is *still current*.
See [the open tip](#the-open-tip) for why it is not written out.

Storing one integer rather than every revision a version appeared in is what
keeps the file flat as revisions accumulate; otherwise a package that never
changes version gains an entry per revision forever. Measured across encodings
at 109 revisions:

| encoding | size | grows with revision count? |
|---|---|---|
| full revision list, names | 63.9 MB | yes |
| `[first, last]`, offsets | 4.1 MB | no |
| newest only, offset | **3.3 MB** | no |

Newest is also the build-correct choice: it is the most patched build of that
version, and the one Hydra produced most recently, so the most likely to still
substitute from the cache.

As of 2026-08-10 that file is 5.45 MB and covers **304,758 (attribute, version) pairs
across 31,798 attributes**.

`index/history.json` answers the question `versions.json` deliberately cannot:
not "where can I get this version" but "when did this version exist". It stores
each version's lifetime as runs of revision offsets, nesting only when a version
left and came back:

```json
{ "hello": { "2.12.3": [1495, null], "2.10": [[1, 723], [728, 728]] } }
```

A run that is still open ends in `null`, the same claim `versions.json` makes
with a null offset: this version is current as of `revisionCount - 1`.

### The open tip

Both files are appended to hourly, and both are in git, so what matters is not
only how large they are but how much of them a single revision *changes*.

Closing those runs literally — writing `[1495, 1539]` and bumping it to
`[1495, 1540]` next hour — makes an append rewrite the entry of every version
that did not change. On a typical revision that is 24,854 of 31,819 attributes:
about 3 KB of real news, scattered as thousands of four-byte edits across a
5.4 MB file. Git stores that as a ~140 KB delta per file per revision, roughly
50× the size of the change, and it accumulates in the history forever.

Leaving the tip open costs a subtraction at read time and takes the two files
from ~284 KB of pack per indexed revision to ~6 KB:

| | pack growth per revision |
|---|---|
| `versions.json`, tip written out | 141 KiB |
| `versions.json`, tip left open | **3 KiB** |
| `history.json`, tip written out | 143 KiB |
| `history.json`, tip left open | **3 KiB** |

Readers resolve a null against the `revisionCount` of the file that carries it,
never against `length revisions`. The two disagree in the ordinary window
between `fetch-unstable-revisions.sh` appending a revision and `build-index.sh`
indexing it, and in that window the newly appended revision is one the index has
never evaluated — resolving to it would claim a version was current in a tree
nobody looked at.

`releases.json` is separate and indexed by nothing: 25 release channels, each
holding the current tip of its branch. Releases move as backports are applied, so a release is a channel, not a snapshot, and it lives outside the revision array for that reason. See [releases move, revisions do not](./nix-api.md#releases-move-revisions-do-not).

## Grouped pins

`version` resolves one (attribute, version) pair to the newest revision that
shipped it. Hand a *set* of pins to that rule one at a time and the worst
case is one revision per pin. Five pins mean five individual nixpkgs fetches
and scans, even when a single revision carried every requested version at once.

The history index records every revision each version was present in, so
resolving a set is a covering problem: pick points on the revision axis such
that every pin's runs contain at least one, and as few points as possible. In
full generality that is set cover, so `resolvePins` plans with the standard
greedy, repeatedly taking the revision that satisfies the most unresolved
pins, with two deliberate preferences layered on:

- **Fewer revisions beat freshness.** A revision costs a fetch and an
  evaluation; a version served from earlier in its own lifetime is the same
  version. Freshness only breaks ties.
- **Ties go to the newest revision**, for the same reason the index keeps
  newest-only: the most patched build, the most likely to still substitute.

Only run *ends* are ever candidates. A minimum piercing never needs an
interior point; any pierce point slides right to the nearest end of a run
containing it without leaving any run it was in. Run ends are observed
sightings, where an interior offset can be one the extractor skipped and the
run merely bridges.

Two consequences follow. A pin that cannot share resolves to exactly what
`version` picks, so nothing regresses by being planned. A pin that does share
may be served by an older build of its version than `version` would choose:
same version, same upstream source, older surrounding dependencies. That is
inherent to sharing a revision at all.

Planning reads `history.json`, which `version` deliberately never pays for
(see [the index](#the-index)): ~40-50 ms of `fromJSON`, against whole
revisions not fetched. A single pin skips the planner entirely and keeps
`version`'s exact cost and behaviour.

---

For the longer version of this story, see the blog post:
[nixpkgs-multiverse: every version that ever
existed](https://fzakaria.com/2026/08/09/nixpkgs-multiverse-every-version-that-ever-existed).
