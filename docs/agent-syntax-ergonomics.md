# Agent Syntax Ergonomics — explicit delimiters, not a translator language

A design note on a recurring question: since Aven is whitespace-significant
(layout pass over indentation), would it help LLM agents to read and write code
through an alternate, brace-explicit syntax — translate the file into an
agent-friendly form on read, translate back to the human layout form on write?

Short answer: the instinct is sound and has real prior art, but the *full* form
— a separate agent language plus a bidirectional translator — is the wrong
shape. A cheaper thing that rides on infrastructure Aven needs anyway
(idempotent formatter, eventual lossless CST) captures most of the benefit
without the worst failure mode. This note records the reasoning so we don't
re-derive it, and states the decision.

## The problem this would solve

For an LLM, the friction with significant-whitespace languages is not *reading*
or *reasoning* — indentation arguably makes human visual structure clearer. The
friction is in *writing* and especially *restructuring* edits, and it has one
specific cause: with delimiter languages, block structure is encoded in
characters the model emits explicitly; with layout languages it lives in
whitespace the model must keep consistent across edits it cannot see rendered.

Two consequences:

- **No error-detecting redundancy.** A misplaced `}` is a syntax error a few
  tokens later. A wrong indent level is frequently *still valid code with
  different semantics* — the binding attaches to the wrong scope, a statement
  falls out of a block. Braces are an error-detecting code; indentation is not,
  so the model has nothing local to self-check against.
- **Edits don't compose locally.** Wrapping a block in a new scope shifts every
  line inside it. Token-by-token generation commits to indentation before the
  block's depth is "seen."

Haskell is worse than Python here: the offside rule, implicit `{ ; }` insertion
driven by the column of the first token after `let`/`where`/`do`/`of`, and the
parse-error(t) rule are genuinely subtle. This is the same property that makes
the layout pass a soundness landmine for incremental reparse (see Milestone 9):
an indentation edit can change block structure far from the edit site.

**But the problem is marginal.** The friction is narrow — restructuring edits,
invisible tabs vs spaces, Haskell-grade offside subtleties — not comprehension.
That sets a high bar for spending real complexity to solve it.

## What the idea actually is: projectional editing

"One program, multiple surface syntaxes that are views over a shared structure"
is **projectional editing** (JetBrains MPS, Unison, Lamdu), or more narrowly
*dual concrete syntax over one AST*. Lisp has lived here forever: "sweet
expressions" (SRFI-110) are an indentation-sensitive skin over s-expressions.
And Haskell itself already has the brace-explicit form as a first-class citizen
— layout is *defined* as sugar over explicit `{ ; }`, and you may write either.

The strongest prior-art signal is the closest analogy: the JavaScript
semicolon debate (ASI-minimal vs explicit). The industry did **not** resolve it
with a live bijection between the two forms. It resolved it with
**canonicalization** — Prettier/standard pick one form and everyone runs the
formatter. Nobody maintains a round-trippable translator between semicolon-ful
and semicolon-less JS, because two stored forms of the same code create churn
and edge cases. The stable equilibrium is "one canonical form + an idempotent
formatter," not "two forms + a translator."

## The constraint that decides everything: what the round-trip passes through

A read → edit → write-back cycle funnels through some intermediate
representation, and **what survives the trip is exactly what that representation
captures.**

- **Through the AST** → everything the AST does not model is lost: comments,
  blank-line grouping, intentional alignment, trailing-comma style. That is the
  gofmt tradeoff — fine when a user opts in, but here it would be imposed on the
  human every time an agent touches the file.
- **Through a lossless CST** (trivia attached to nodes — Roslyn red-green,
  rust-analyzer's rowan) → faithful, but that is exactly the expensive machinery
  Milestone 9 defers, and the same machinery a real CST would require.

And the failure mode dominates the upside. A translator bug does not produce an
occasional indent slip the way an unaided model does — it corrupts files
**silently and systematically**, in both directions, on code that already
worked. That is a strictly worse class of bug than the marginal one it prevents.
For a marginal problem the trade is bad unless the translator is provably total
and faithful.

## Where the translation lives changes the design

Two different proposals hide in the question:

1. **Storage-layer translation** — the file on disk flips to the
   agent-friendly form. Non-starter: git churn, two camps reformatting each
   other's commits forever.
2. **Editing-boundary translation** — disk stays human-canonical; the agent's
   *view* is translated on read and its *edits* translated on write, but the
   stored artifact never changes form. This is the right shape (and what the
   question actually described). Cost: write-back must be a *total, faithful
   inverse*, and surgical edits are the hard case — mapping an edit on the
   translated view back to a byte range in the original needs precise span
   correspondence. Whole-file rewrites are easy; partial edits are where it
   bites, and partial edits are the common case.

## The Aven shortcut

Aven already has the brace-explicit form internally: it is the `layout_tokens`
stream — `Indent`/`Dedent`/`Newline` are the explicit block delimiters the
layout pass synthesizes from whitespace. We do not need to invent a second
language; we have one half of the translator already, and Haskell shows the
other half is a language-design choice rather than new machinery.

So the cheap version is not a translator. It is:

1. **Optionally accept explicit block delimiters as equivalent input.** One
   grammar, one AST; the layout pass already understands the explicit form. An
   agent that wants zero ambiguity *writes* the explicit form, and the parser
   accepts it identically to the indented form. (Mirrors Haskell's `{ ; }`.)
2. **Make the formatter total and idempotent**, canonicalizing to the
   indentation form.

Now the formatter Aven is building anyway *is* the "translate back to human"
step. The agent chooses the unambiguous input; the human reads layout; the
formatter is the bridge. No second parser, no bijection to maintain, no new
corruption surface — explicit delimiters are simply valid input the formatter
normalizes away. This is a spec change that *shrinks* the problem rather than
adding a parallel system, in line with the project's bias toward proposing
language changes that simplify the implementation.

## Decision

- **Do not build a translator language or an editing-boundary projection now.**
  The problem is marginal and the failure mode (silent, systematic file
  corruption from a translator bug) is worse than the indent slips it prevents.
- **Build the two things tooling wants regardless**, and let them subsume most
  of the benefit:
  - a **trivia-preserving / lossless CST** when Milestone 9's triggers fire —
    it is the prerequisite for *any* faithful projection, so projectional
    editing must wait for it;
  - an **idempotent, total formatter** (Milestone 5) — the canonical-form
    bridge. Idempotence and totality are load-bearing for this idea, not just
    formatter hygiene.
- **Optionally expose explicit block delimiters as equivalent input syntax**
  (Haskell-style `{ ; }` over the layout pass) if and when an agent-authoring
  workflow wants an unambiguous emit form. This is the low-cost ~90% of the
  benefit and rides entirely on the existing layout pass plus the formatter.
- **Keep editing-boundary projection in the back pocket.** Revisit only once a
  lossless CST exists (so write-back is a faithful inverse) *and* surgical-edit
  span mapping is solid. Before that it is a liability.

## Open questions, if a trigger is hit

- Does the layout pass round-trip cleanly from explicit delimiters back to
  canonical indentation for *all* block forms (lambda bodies, match arms, record
  rows, pipelines), or are there forms where the explicit form admits structure
  the indented form cannot express? Any such gap is where a future projection
  would lose faithfulness.
- If editing-boundary projection is ever built, the inverse must be checked the
  way a formatter's idempotence is: `to_human(to_agent(src)) == format(src)` as
  a property test over the fixture corpus, not spot checks.

See also: Milestone 5 (formatter idempotence/totality) and Milestone 9 (the CST
deferral and the layout-pass soundness landmine) in
[`tooling-first-plan.md`](tooling-first-plan.md).
