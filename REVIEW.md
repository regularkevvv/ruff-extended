# Review decisions

Only regressions introduced by this branch are in scope. Each finding must have a focused
reproduction and a comparison against `main` before it is accepted.

- **Fixed — inherited nominal compatibility:** Materialization changes the inherited `Any` member to `object`, so accepting the source through its stale nominal origin was introduced by this branch; materialized sources now use their effective interface.
- **Fixed — recursive aliases:** Direct recursion stayed class-backed, but the branch synthesized a non-idempotent interface when the same recursion was hidden behind an alias; recursion detection now follows alias values without walking protocol interfaces.
- **Fixed — property descriptor identity:** `main` retains the property's `int` read after assignment, while the branch reduced the descriptor to a value and narrowed it to `Literal[1]`; assignment narrowing now reuses descriptor-aware assignment lookup.
- **Pending — generic inference:** Determine whether inference reads an unmaterialized protocol origin instead of its effective interface.
- **Pending — generator delegation:** Determine whether `yield from` derives stale types from a materialized protocol's origin.
