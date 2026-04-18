# sample-py-repo

A tiny three-file Python project used to demonstrate AgentStateDeveloper's
indexer and effect inference. Each file targets a different effect profile:

- `greetings.py` — pure functions, no effects.
- `payments.py` — `io.db.read`, `io.db.write`, `io.net`, `log`, `throw`.
- `logger.py` — `io.fs.read`, `io.fs.write`, `time.read`, `env.read`.

## Try it

```
asd init
asd index .
asd read payments.charge_card
asd ledger append payments.charge_card --kind hazard --summary "raises on amounts >10000; caller must catch"
asd verify-effects payments.charge_card
```

After indexing, the `.asd/` directory will hold the symbol map and effect
summaries. No fixtures are committed — everything is generated on demand.
