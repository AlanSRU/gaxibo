# Forks and gaps

Three Linux players share the base commit `d47fb25`: upstream
[Arexibo](https://github.com/birkenfeld/arexibo), Gaxibo (this fork), and
[romoloman/arexibo](https://github.com/romoloman/arexibo). They have diverged
in different directions, and the same bug has been found more than once.

    tools/fork-status.py --fetch

prints the live comparison. **Do not read the tables in this file as current** —
the capability matrix is derived from the three trees on every run, so the
script is the source of truth and this file holds only what cannot be derived:
a verdict per commit, and the gap against the CMS.

Add the remotes once:

    git remote add upstream  https://github.com/birkenfeld/arexibo
    git remote add romoloman https://github.com/romoloman/arexibo

## Why a ledger at all

Romoloman's fork is 98 commits and ~19.7k insertions past the base. Only **4 of
the 72 substantive commits cherry-pick onto upstream** — from the second commit
onward each builds on the last — so the extractable unit is a *behaviour*
re-implemented against upstream, not a commit. That means the mapping between
"their commit" and "our position on it" cannot be computed, and has to be
recorded.

Two traps the ledger exists to stop us re-learning:

- **Fixes of their own regressions.** "Fix regression in layout navigation",
  "fix reggression on webpage render=native", "Fix regression on resync" —
  upstream has no such regression, so these are meaningless there.
- **Fixes arrive in chains.** Fly transitions is `867f854` + `876f714`;
  playlists are `f765f99` + `a1287a1` + `07a72ea`; resync is four commits. One
  PR is two to four of their commits folded together.

Verdicts: **ours** (we have equivalent behaviour) · **wanted** (worth having,
upstream-relevant) · **feature** (a project, not a chunk; likely the
maintainer's architectural call) · **n-a** (fork-specific or a self-regression)
· **upstreamed** (sent; note the PR).

## Ledger

Only commits actually assessed appear here. Everything else shows up under
"Untriaged" when the script runs — deliberately, because a verdict nobody has
formed is worse than a blank.

| Commit | Fork | Subject | Verdict | Note |
|---|---|---|---|---|
| `42fc9e6` | romoloman | apply CMS-configured logLevel instead of ignoring it | wanted | upstream ignores a setting it collects |
| `026365f` | romoloman | fix allow-offline option does not work | wanted | upstream's own flag, broken |
| `d149775` | romoloman | double registration of cryptoprovider causing a crash | wanted | rustls; a crash, so easy to justify |
| `715cefe` | romoloman | fix TLS certificate validation | wanted | security-relevant; verify before sending |
| `f816c13` | romoloman | Implement XMR reconnect retry | wanted | we hit this: the ZMQ connector never reconnects |
| `576831c` | romoloman | Handle empty xmr address | wanted | pairs with the above |
| `0d4c70e` | romoloman | XMR Address override fix | wanted | pairs with the above |
| `3ea0b0c` | romoloman | Fix growscale and shrinkscale | wanted | upstream layout feature, wrong output |
| `85fc520` | romoloman | Increase timeout / retry for RSS and dataset resources | wanted | 15 lines |
| `40e720a` | romoloman | Fix Handling of key press | wanted | small, self-contained |
| `f765f99` `a1287a1` `07a72ea` | romoloman | playlist duration and loop fixes | wanted | fold together; **overlaps our own held duration fix** — diff before sending |
| `00ef623` | romoloman | NotifyStatus not called on a fresh registration | wanted | **read first**: bears directly on the timezone lead in `LEDPlayer/gaxibo/PLAN.md` |
| `eb0c24a` | romoloman | Implement and obey display_time_zone setting | wanted | same; they already solved what we hit tonight |
| `867f854` `876f714` | romoloman | Implement fly transitions, and its fix | wanted | neither we nor upstream implement transitions at all |
| `4bf50ed` | romoloman | Fix regression in layout navigation | n-a | regression in their own change |
| `df67619` | romoloman | fix regression on webpage render=native | n-a | as above |
| `badb990` | romoloman | Fix regression on resync | n-a | as above; belongs to syncgroup |
| `907197e` and the v7 chain | romoloman | migrate to xmds v7 | feature | architectural; the maintainer's call, not ours to pre-empt |
| `e26ac17` `b34a917` | romoloman | syncgroup implementation | feature | new module; CMS has `/syncgroup/*` |
| `f128b28` `206d3a2` `defb9a6` `d6f7393` | romoloman | webhook and interactive actions | feature | CMS has `/action/*` |
| `89fb933` | romoloman | scheduled commands | feature | CMS has `/displaygroup/{id}/action/command` |
| `0815898` | romoloman | Create .deb package | n-a | already open upstream as #22 by another contributor |
| `a6c4309` | **ours** | schedule navigate after a single completed layout | upstreamed | [arexibo#36](https://github.com/birkenfeld/arexibo/pull/36) |
| `5b6e078` | **ours** | end a video region on `ended`, not a sampled duration | wanted | drafted on the PR fork, not yet sent |

### Before extracting anything

Upstream's last commit is **2026-04-26**, and two obviously-mergeable PRs from
another contributor (#21, #22 — a systemd unit and packaging) have been open
since **2026-04-02**. The landing zone may not exist. Send one or two and use
[#36](https://github.com/birkenfeld/arexibo/pull/36) as the canary before
spending the ~20–35 hours the "wanted" list would take.

And ask romoloman first: it is their work under the AGPL, they may have context
or reasons of their own, and offering to do the PR work for them is the decent
opening. Preserve authorship — a cherry-pick keeps it, and a re-implementation
should carry `Co-authored-by:` naming the source commit.

## Gaps against the official players

Xibo's own players are Windows, Android, webOS, Tizen and Chrome OS; Linux is
community-only, which is why all three forks here exist. Rather than claim what
those players do — which would be assertion — this table is grounded in **what
the CMS exposes**, queried from a live Xibo 4.5.1 (`/swagger.json`,
`GET /module`). If the CMS offers it, a player is expected to honour it.

| Capability | CMS evidence | Ours | romoloman |
|---|---|---|---|
| `changeLayout` XMR action | `/displaygroup/{id}/action/changeLayout` | **no** | yes |
| `revertToSchedule` XMR action | same family | **no** | yes |
| `overlayLayout` XMR action | same family | **no** | yes — but needs two composited surfaces, which the dmabuf constraint forbids on RK3399 |
| `dataUpdate` / `criteriaUpdate` | `/playlist/widget/data/{id}`, criteria | **no** | yes |
| Sync groups | `/syncgroup/add`, `/syncgroup/{id}/displays` | **no** | yes (`syncgroup.rs`) |
| Interactive actions / webhooks | `/action`, `/action/{id}` | partial (`triggerWebhook` only) | yes |
| Ad campaigns | `/campaign`, campaign type `ad` | **no** | yes (`adspace.rs`) |
| Fault reporting | XMDS `ReportFaults` | **no** | yes (`faults.rs`) |
| Proof of play detail | `/stats`, `/stats/timeDisconnected` | basic | yes (`stats.rs`) |
| Scheduled commands | `/displaygroup/{id}/action/command` | **no** | yes |
| Transitions (fade, fly) | per-widget `transitionType` in the XLF | **no** | yes |
| `audio` widget | module `core-audio`, `renderAs=native` | **no** | yes |
| `videoin` widget | module `core-videoin` | **no** | **no** |
| DataSet v7 `GetData` | `/dataset/data/{id}`, data connectors | **no** | yes |
| Display timezone | reported in `NotifyStatus`; CMS stores per display | reports it; CMS records none | yes (`eb0c24a`) |
| Screenshots | XMR `screenShot`, `/display/screenshot` | Qt path only — **not** under `--renderer wpe` | yes |

Two rows are ours alone and belong in any comparison the other way round:
**hardware video decode** on the RK3399 VPU, and **video in a region** rather
than only full screen. Neither of the other two trees has either.
