# Review decisions

Only regressions introduced by this branch are in scope. Each finding must have a focused
reproduction and a comparison against `main` before it is accepted.

- **Fixed — inherited nominal compatibility:** Materialization changes the inherited `Any` member to `object`, so accepting the source through its stale nominal origin was introduced by this branch; materialized sources now use their effective interface.
- **Fixed — recursive aliases:** Direct recursion stayed class-backed, but the branch synthesized a non-idempotent interface when the same recursion was hidden behind an alias; recursion detection now follows alias values without walking protocol interfaces.
- **Fixed — property descriptor identity:** `main` retains the property's `int` read after assignment, while the branch reduced the descriptor to a value and narrowed it to `Literal[1]`; assignment narrowing now reuses descriptor-aware assignment lookup.
- **Fixed — generic inference:** `main` consistently reads inherited `Any`, but this branch exposes `object` while inference still used the stale nominal origin; materialized protocol actuals now infer through structural constraints.
- **Fixed — generator delegation:** `main` consistently derives `Any`, but this branch exposes `object` from `__next__` while `yield from` still returned stale `Any`; materialized protocols now carry their polarity so inherited generator parameters can be mapped with the correct variance.
- **Fixed — overly broad nominal bypass:** Full-suite validation showed that bypassing nominal checks for every materialized source rejected an existing `TypeIs` narrowing case whose target members were unchanged; structural comparison is now required only when materialization changed a member required by the target.
