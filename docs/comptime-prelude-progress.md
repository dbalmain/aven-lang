# Comptime prelude progress

Slice 2 infrastructure introduces trusted prelude module roots. A prelude is
loaded before the entry graph node; its exported record fields become ordinary
lexical defaults during checking and runtime evaluation. User declarations
still shadow them. The mechanism is generic and has no compiler branch for a
specific prelude name.

`std/prelude.av` currently exports only `identity` while demand-outcome repair
owns the checker changes needed to install `comptime = (@arg) => arg`.
