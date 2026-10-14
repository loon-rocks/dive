# Migrating

Each major release gets a section here, headed `## v<N-1> to v<N>`, saying what changed and how to update plans and agents by hand.

Ship `plans/migrations/migrate_v<N>.yaml` with the release too, taking the inputs `root` and `apply`, and `graph pack update --major` runs it over your users' files. Put sample files written against the previous major in `tests/migrations/v<N>/` and `graph pack test` checks that the migration brings them up to date.
