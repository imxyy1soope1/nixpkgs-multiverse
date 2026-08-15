# Tests grouped pin resolution, pinPlan and resolvePins, against the
# committed index files, by evaluating this file strictly:
#
#   nix eval --json -f tests/pins.nix --apply 'f: f { }'
#
# Nothing here forces a derivation, so nothing is fetched. The planner's whole
# job is to answer, from the two index files alone, which revisions a pin set
# costs and which pin lands where; these tests hold it to that.
{
  system ? "x86_64-linux",
}:
let
  mv = import ../multiverse.nix { inherit system; };

  # Versions old enough to be settled, whose lifetimes are known to overlap:
  # ripgrep 13.0.0, fd 8.7.0 and jq 1.6 were all current at once. Resolved one
  # at a time they cost three revisions, because each version's newest sighting
  # is a different commit. This trio is exactly the case the planner exists
  # for.
  shareable = {
    ripgrep = "13.0.0";
    fd = "8.7.0";
    jq = "1.6";
  };
  shared = mv.pinPlan shareable;

  # hello 2.10 was gone from nixpkgs before htop 3.2.2 arrived, so no revision
  # can serve both and the plan must split.
  disjoint = mv.pinPlan {
    hello = "2.10";
    htop = "3.2.2";
  };

  single = mv.pinPlan { ripgrep = "13.0.0"; };

  # Every pin in every group, flattened back into one attrset.
  planned = plan: builtins.foldl' (acc: g: acc // g.pins) { } plan;
in

# Overlapping lifetimes collapse to one revision carrying every pin.
assert builtins.length shared == 1;
assert (builtins.head shared).pins == shareable;

# The chosen revision genuinely shipped each pinned version: the history index
# must agree with the plan, attr by attr, for every group of every plan. This
# is the honesty property everything else rests on.
assert builtins.all (
  g: builtins.all (attr: mv.versionAt attr g.label == g.pins.${attr}) (builtins.attrNames g.pins)
) (shared ++ disjoint ++ single);

# Ties prefer the newest revision, so the shared revision is the newest point
# where all three lifetimes still overlap: jq 1.6's last sighting.
assert (builtins.head shared).label == mv.revOf "jq" "1.6";

# Disjoint lifetimes split, groups arrive newest first, and a pin that cannot
# share resolves to the newest revision shipping its version, exactly what
# `version` picks for a lone pin. Nothing regresses by being planned.
assert builtins.length disjoint == 2;
assert (builtins.head disjoint).pins == { htop = "3.2.2"; };
assert (builtins.head disjoint).label == mv.revOf "htop" "3.2.2";
assert (builtins.elemAt disjoint 1).label == mv.revOf "hello" "2.10";

# A one-pin plan is the old behaviour with a list around it.
assert builtins.length single == 1;
assert (builtins.head single).label == mv.revOf "ripgrep" "13.0.0";

# No pin is dropped or invented: the groups partition the input.
assert planned shared == shareable;
assert
  planned disjoint == {
    hello = "2.10";
    htop = "3.2.2";
  };

# resolvePins mirrors the pins' keys without forcing any value; forcing is
# what fetches, and the shape must be inspectable before paying that.
assert builtins.attrNames (mv.resolvePins shareable) == builtins.attrNames shareable;

# Nothing to resolve is nothing to plan, not an error.
assert mv.pinPlan { } == [ ];
assert mv.resolvePins { } == { };

# A version the index has never seen fails the plan loudly, with the same
# report `version` gives.
assert !(builtins.tryEval (mv.pinPlan { ripgrep = "0.0.0-never"; })).success;

{
  sharedGroups = builtins.length shared;
  sharedLabel = (builtins.head shared).label;
  disjointGroups = builtins.length disjoint;
}
