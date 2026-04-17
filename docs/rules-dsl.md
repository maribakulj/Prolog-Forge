# Rules DSL — Prolog Forge (Datalog v1)

The surface syntax is Prolog-flavored for ergonomics. The semantics are pure
Datalog v1: no function symbols, no negation, no aggregates. Every program
terminates.

## Grammar

```text
program ::= clause*
clause  ::= atom "." | atom ":-" atom ("," atom)* "."
atom    ::= ident "(" term ("," term)* ")"
term    ::= VAR | CONST
VAR     ::= [A-Z_] [A-Za-z0-9_]*
CONST   ::= ident | quoted_string | integer
ident   ::= [a-z] [A-Za-z0-9_]*
```

Line comments start with `%` or `//`. Predicate names start with a lowercase
letter. Variables start with an uppercase letter or underscore.

## Example — transitive closure

```prolog
parent(alice, bob).
parent(bob, carol).
parent(carol, dan).

ancestor(X, Y) :- parent(X, Y).
ancestor(X, Z) :- parent(X, Y), ancestor(Y, Z).
```

Running this program to fixpoint derives six `ancestor/2` facts.

## Semantics (Phase 0)

- **Evaluation:** bottom-up, naive fixpoint. Every rule is fired against the
  current fact set until no new derivation occurs. Correct and terminating
  for any Datalog program.
- **Epistemic layer:** ground facts declared in source are stored as
  `observed`. Facts derived by the engine are stored as `inferred`.
- **Deduplication:** the graph is a set, not a multiset; re-deriving an
  existing fact is a no-op.
- **Arity checking:** the first use of a predicate fixes its arity; later
  clauses using a different arity are rejected.

## Planned extensions

- **Semi-naive incremental evaluation** (Phase 1). Matches the Phase 0 API
  unchanged.
- **Stratified negation** and **aggregates** (`count`, `min`, `max`, `sum`) —
  Phase 2.
- **Weighted / probabilistic candidate rules** — Phase 3, used only for L2
  `candidate` facts, never for L3 inference.
- **Constraints** (integrity clauses whose violation raises a diagnostic) —
  Phase 2, materialized as L4.
- **Built-ins** (equality, comparison, string ops) — Phase 1, implemented as
  sandboxed Rust functions registered into the evaluator.

## Non-goals

- No higher-order syntax. No `findall`, no meta-call.
- No cuts, no side-effects, no mutable state.
- No file includes in the source language itself — composition of rule packs
  is a responsibility of the Core, not the parser.
