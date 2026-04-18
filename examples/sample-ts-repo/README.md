# sample-ts-repo

A tiny four-file TypeScript project used to demonstrate AgentStateDeveloper's
indexer and effect inference on TS. Each file targets a different effect
profile:

- `src/greetings.ts` — pure functions, no effects.
- `src/payments.ts` — `io.db.read`, `io.db.write`, `log`, `throw`.
- `src/logger.ts` — `io.fs.read`, `io.fs.write`, `time.read`, `env.read`.
- `src/driver.ts` — cross-module edges via `import * as` and named imports.

## Try it

```
asd init
asd index src/
asd read driver.main
```

After indexing, the `.asd/` directory will hold the symbol map and effect
summaries. No fixtures are committed — everything is generated on demand.
