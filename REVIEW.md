# Review decisions

Only regressions introduced by this branch are in scope. Each finding must have a focused
reproduction and a comparison against `main` before it is accepted.

- **Fixed — inherited nominal compatibility:** Materialization changes the inherited `Any` member to `object`, so accepting the source through its stale nominal origin was introduced by this branch; materialized sources now use their effective interface.
- **Pending — recursive aliases:** Determine whether aliases hide recursive protocol origins and break materialization idempotence.
- **Pending — property descriptor identity:** Determine whether materialized properties are incorrectly narrowed after assignment.
- **Pending — generic inference:** Determine whether inference reads an unmaterialized protocol origin instead of its effective interface.
- **Pending — generator delegation:** Determine whether `yield from` derives stale types from a materialized protocol's origin.
