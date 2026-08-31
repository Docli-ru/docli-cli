# Contributing to Docli CLI

Спасибо за интерес! / Thanks for your interest! This document is honest about how contributions
work here, because the flow is unusual.

## The import flow (no direct merges)

Development happens in docli's private monorepo; each release mirrors the source here
(GitVerse [agitek/docli-cli](https://gitverse.ru/agitek/docli-cli) is the PRIMARY repository;
GitHub [Docli-ru/docli-cli](https://github.com/Docli-ru/docli-cli) is the mirror). Pull
requests are therefore never merged directly — they are **imported**: after review and the CLA
(below), your commit is applied into the monorepo with `git am --author` (your authorship
preserved) + `Co-authored-by`, runs the full monorepo test gate, and ships with the next
release. The PR is then closed with a pointer to that release.

Wire-crate (`docli-sync-wire`) PRs are refused by default: a protocol proposal starts
server-side — open an issue instead.

## CLA — REQUIRED, and currently in legal review

Outbound, this code is MIT. Inbound contributions require a short individual CLA: you keep
ownership of your contribution and grant OOO Agitek a perpetual, irrevocable, sublicensable
right to use, modify, distribute and RELICENSE it under any terms, commercial included, plus an
explicit patent license. (MIT alone covers commercial USE but grants no patent rights and no
attribution-free relicensing — that is why the CLA exists.)

**The CLA text is being reviewed by counsel and is not final yet. Until it is published here,
external PRs are deferred** — feel free to open issues, and PRs will wait in the queue rather
than being reviewed. This paragraph is replaced by the CLA checkbox instructions once counsel
clears the text.

## Style

Match the code around you: sparse, human comments that state constraints the code can't; SPDX
headers on every source file; tests next to the code they pin.
