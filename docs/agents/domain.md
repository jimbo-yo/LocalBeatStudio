# Domain Docs

How the engineering skills should consume this repo's domain documentation when exploring the codebase.

LocalBeat Studio is one of three repos in the LocalBeat suite (this one, `../LocalBeat` for the Android app, `../LocalBeatServer` for the Mac Mini server). They share one vocabulary, kept in a single glossary in the LocalBeat repo, not here.

## Before exploring, read these

- **`../LocalBeat/GLOSSARY.md`**: the suite's single glossary (Queue, not Up Next; music server; CD rip; Kept vs Synced...). Its plan for the shared interface is `../LocalBeat/docs/ui-refinement.md`. Don't create a `GLOSSARY.md` in this repo: new or changed terms go into the LocalBeat one, in step with the other two repos.
- **`docs/adr/`** in this repo: decisions specific to LocalBeat Studio. Suite-wide contracts (library layout, tags, phone record, Opus settings, playlists / users-and-devices) are recorded in each repo's `CLAUDE.md` and in `../LocalBeat/docs/control.md`.
- **`CLAUDE.md`** and **`docs/*.md`** here: design notes (`rust-app-design.md`, `server-library.md`, `joining.md`, `ctdb-repair.md`, `ui-redesign.md`) and the journal `history.md`, which holds the reasons behind most rules.

If the sibling repo isn't checked out next to this one, or `docs/adr/` doesn't exist yet, **proceed silently**. Don't flag their absence; don't suggest creating them upfront. The `/domain-modeling` skill (reached via `/grill-with-docs` and `/improve-codebase-architecture`) creates ADRs lazily when decisions actually get resolved.

## File structure

```
Development/
├── LocalBeat/
│   └── GLOSSARY.md          ← the one glossary for the suite
├── LocalBeatServer/
└── LocalBeatStudio/         ← this repo
    ├── CLAUDE.md
    └── docs/
        ├── adr/             ← Studio-specific decisions
        │   └── 0001-....md
        └── agents/
```

## Use the glossary's vocabulary

When your output names a domain concept (in an issue title, a refactor proposal, a hypothesis, a test name, UI text), use the term as defined in `../LocalBeat/GLOSSARY.md`. Don't drift to synonyms the glossary explicitly avoids.

If the concept you need isn't in the glossary yet, that's a signal: either you're inventing language the project doesn't use (reconsider) or there's a real gap (note it for `/domain-modeling`, which should add it to the LocalBeat glossary).

## Flag ADR conflicts

If your output contradicts an existing ADR, surface it explicitly rather than silently overriding:

> _Contradicts ADR-0007 (...), but worth reopening because…_
