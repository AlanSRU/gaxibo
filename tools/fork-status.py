#!/usr/bin/env python3
"""Compare Gaxibo, upstream Arexibo and romoloman's fork.

Run this at the start of any session on the player. It answers two questions
that are expensive to answer from memory and easy to get wrong:

  - has the thing I am about to fix already been fixed in one of the others?
  - what has landed in either of them since I last looked?

**Everything in the matrix is derived from the three trees**, read with
`git show <ref>:<path>` so nothing is checked out. A hand-written table would
be wrong within a month; this one is wrong only if the extraction is, and the
extraction is a dozen lines you can read below.

The one hand-maintained part is the ledger in FORKS.md, which records a verdict
per commit. This script lists commits with no verdict, which is the work queue.

    tools/fork-status.py            # report
    tools/fork-status.py --fetch    # fetch the remotes first
    tools/fork-status.py --new      # only the untriaged commits
"""
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
LEDGER = ROOT / "FORKS.md"

# name -> ref.  "ours" is the working branch, so the report describes the tree
# you are actually sitting on rather than the last push.
TREES = {
    "upstream": "upstream/master",
    "romoloman": "romoloman/master",
    "ours": "HEAD",
}


def git(*args, check=True):
    r = subprocess.run(["git", *args], cwd=ROOT, capture_output=True, text=True)
    if check and r.returncode != 0:
        raise SystemExit(f"git {' '.join(args)} failed:\n{r.stderr.strip()}")
    return r.stdout


def show(ref, path):
    """A file from a ref, or None if that tree has no such file."""
    r = subprocess.run(["git", "show", f"{ref}:{path}"],
                       cwd=ROOT, capture_output=True, text=True)
    return r.stdout if r.returncode == 0 else None


def check_remotes():
    have = set(git("remote").split())
    want = {r.split("/")[0] for r in TREES.values() if "/" in r}
    missing = want - have
    if missing:
        print("Missing remotes: " + ", ".join(sorted(missing)))
        print("  git remote add upstream  https://github.com/birkenfeld/arexibo")
        print("  git remote add romoloman https://github.com/romoloman/arexibo")
        raise SystemExit(1)


# ─── derived capabilities ────────────────────────────────────────────────────
# Each extractor takes a ref and returns a set of strings. They are regexes
# over source rather than anything cleverer on purpose: the three trees have
# diverged enough that no shared structure can be relied on, and a regex that
# stops matching shows up as an empty cell rather than a wrong one.

def xmr_actions(ref):
    """XMR action names the player acts on, from the `into_msg` match."""
    src = show(ref, "src/xmr.rs") or ""
    return set(re.findall(r'^\s+"([a-zA-Z]+)"\s*=>', src, re.M))


def _arm_values(group):
    """Split a match arm's `"a" | "b"` alternatives into names."""
    return {t.strip().strip('"') for t in re.split(r'"\s*\|\s*"', group) if t.strip()}


def widget_types(ref):
    """Widget types `write_media` renders.

    Position matters: `write_media` matches on the tuple
    `(render, type)`, so the *second* slot is the widget type. An earlier
    version of this swept up every `Some("...")` in the file and reported
    "fly" and "native" as widget types, which they are not -- one is a
    transition and the other a render mode.
    """
    src = show(ref, "src/layout.rs") or ""
    found = set()
    for group in re.findall(r'\(_,\s*Some\("([a-z|"\s]+)"\)\)', src):
        found |= _arm_values(group)
    return found


def render_modes(ref):
    """`render` attribute values handled -- the first slot of the same tuple."""
    src = show(ref, "src/layout.rs") or ""
    found = set()
    for group in re.findall(r'\(Some\("([a-z|"\s]+)"\),\s*_\)', src):
        found |= _arm_values(group)
    return found


def transitions(ref):
    """Xibo transition names that appear at all in the layout writer.

    Neither upstream nor Gaxibo implements transitions, so this is expected to
    be empty for both -- an empty column is the finding, not a broken
    extractor.
    """
    src = show(ref, "src/layout.rs") or ""
    return {n for n in ("fadeIn", "fadeOut", "fly") if f'"{n}"' in src}


def modules(ref):
    """Source modules present, which is how features announce themselves."""
    out = show(ref, "src") or ""
    return {ln.strip() for ln in out.splitlines() if ln.strip().endswith(".rs")}


def xmds_versions(ref):
    """XMDS WSDL versions the tree carries."""
    out = git("ls-tree", "-r", "--name-only", ref, check=False)
    return set(re.findall(r"xmds_v(\d+)\.wsdl", out))


CAPABILITIES = [
    ("XMR actions", xmr_actions),
    ("Widget types", widget_types),
    ("Render modes", render_modes),
    ("Transitions", transitions),
    # Read this one with care: a module is how a *large* feature announces
    # itself, but its absence is not proof the feature is missing. Upstream
    # keeps proof-of-play in mainloop.rs, so a missing stats.rs says only that
    # the code is not in a file of that name.
    ("Modules", modules),
    ("XMDS", xmds_versions),
]


def matrix():
    for title, extract in CAPABILITIES:
        sets = {name: extract(ref) for name, ref in TREES.items()}
        every = sorted(set().union(*sets.values()))
        if not every:
            print(f"\n── {title} ──\n  (extraction found nothing -- has the "
                  f"source shape changed?)")
            continue
        print(f"\n── {title} ──")
        print(f"  {'':28} {'ours':>6} {'upstream':>9} {'romoloman':>10}")
        for item in every:
            row = [("yes" if item in sets[n] else "-") for n in
                   ("ours", "upstream", "romoloman")]
            # Flag what the others have and we do not: that is the column
            # this whole script exists to produce.
            gap = "  <-- missing here" if row[0] == "-" else ""
            print(f"  {item:28} {row[0]:>6} {row[1]:>9} {row[2]:>10}{gap}")


def divergence():
    print("── Divergence ──")
    base = git("merge-base", TREES["upstream"], TREES["romoloman"]).strip()
    print(f"  common base: {git('log', '-1', '--format=%h %ad %s', '--date=short', base).strip()}")
    for name, ref in TREES.items():
        counts = git("rev-list", "--left-right", "--count", f"{base}...{ref}").split()
        last = git("log", "-1", "--format=%h %ad", "--date=short", ref).strip()
        print(f"  {name:10} {counts[1]:>4} commits past the base, last {last}")


# ─── the ledger ──────────────────────────────────────────────────────────────

def triaged():
    """Short SHAs already given a verdict in FORKS.md."""
    if not LEDGER.exists():
        return set()
    return set(re.findall(r"\b([0-9a-f]{7,12})\b", LEDGER.read_text()))


NOISE = re.compile(
    r"update readme|nightly-deb|test\.yml|githus|arexibo\.service|lint fix|"
    r"clippy|^merge branch|comment cleanup|^update (mainloop|server|config|"
    r"xmds|xmr)\.rs$|^create nightly|bump to version", re.I)


def untriaged(show_all=False):
    known = triaged()
    base = git("merge-base", TREES["upstream"], TREES["romoloman"]).strip()
    total = new = 0
    lines = []
    for ref in ("romoloman/master", "upstream/master"):
        out = git("log", "--reverse", "--format=%h|%s", f"{base}..{ref}")
        for row in out.splitlines():
            if not row.strip():
                continue
            sha, subject = row.split("|", 1)
            if NOISE.search(subject.strip()) and not show_all:
                continue
            total += 1
            if sha in known:
                continue
            new += 1
            lines.append(f"  {ref.split('/')[0]:10} {sha} {subject[:70]}")
    print(f"\n── Untriaged ({new} of {total} substantive commits have no "
          f"verdict in FORKS.md) ──")
    for ln in lines:
        print(ln)
    if new:
        print("\n  Give each a verdict in FORKS.md: ours / wanted / n-a / upstreamed.")
    return new


if __name__ == "__main__":
    check_remotes()
    if "--fetch" in sys.argv:
        for remote in ("upstream", "romoloman"):
            print(f"fetching {remote}...")
            git("fetch", "--quiet", remote)
    if "--new" in sys.argv:
        untriaged()
    else:
        divergence()
        matrix()
        untriaged()
