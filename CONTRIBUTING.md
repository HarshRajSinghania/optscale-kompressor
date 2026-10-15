# Contributing to OptScale Kompressor

Issues and pull requests are welcome.

## Before you start

Run `make check`. It is exactly what CI gates on — lint, the full test suite,
the corpus replay against its recorded baseline, and the documentation check —
and it is the fastest way to find out whether a change is viable.

```bash
make check
```

## The rules a change has to respect

This project trades flexibility for a guarantee, so a few things are not
negotiable:

1. **Determinism.** The same input must compress to the same bytes on any
   machine, in any later run. The engine crate has no I/O, no clock and no
   randomness; iteration order must not reach the output. A compressor must
   never read the conversation around the payload it is given.
2. **The four invariants.** Never larger, every shrink accounted for,
   deterministic, fail open. They are enforced centrally and checked over the
   whole corpus — a change that violates one fails `make bench`, which is the
   intended outcome.
3. **Measure, then claim.** Comments and docs in this repository carry numbers.
   If you change behaviour, replace the number with the one you measured and say
   how. A plausible claim with no measurement behind it is worse than silence.

## Things that move a gate

`make bench-save` re-records `fixtures/baseline.json`, and `make golden`
re-records the compression vectors. Both change what the tests compare against,
so both prompt before they run. Moving either is a deliberate act: say in the
pull request what moved and why it should.

Adding payloads to `fixtures/corpus.jsonl.gz` also moves the baseline. The
corpus is meant to track what real agent traffic contains, including the
payloads the engine declines — a corpus of things that compress well is a
flattering benchmark, not a gate.

## Documentation

`scripts/docs/check.py` runs as part of `make check`. It verifies that every
documented command and HTTP endpoint exists, that the configuration reference in
`docs/DESIGN.md` is byte-identical to `kompressor check-config --config /dev/null`,
that every `§` cross-reference resolves, and that relative links point at files
that exist. Documentation drift fails the build on purpose.

Diagrams live in `docs/charts/` as `*.drawio.png` — one file each, a PNG that
renders anywhere with its drawio XML embedded in it. Open one in drawio, edit,
save: the picture and the diagram update together, and there is no export step
to forget. `make diagrams` only re-renders them all at the same scale, which is
worth running if you saved one at a different zoom.

## Commits and sign-off

Commits need a Developer Certificate of Origin sign-off:

```bash
git commit -s
```

That adds a `Signed-off-by` line certifying you have the right to submit the
work under this project's licence. See https://developercertificate.org.

## Code of conduct

Everyone taking part in this project — issues, pull requests, reviews and
discussions — is expected to follow the
[Code of Conduct](CODE_OF_CONDUCT.md). Report unacceptable behaviour to
info@hystax.com.

## Security

Please report security issues privately as described in [SECURITY.md](SECURITY.md),
not in public issues. That address is a different one, and deliberately so: a
vulnerability goes to security@hystax.com.

## Licence

Contributions are accepted under the MIT licence, the same as the rest of the
repository.
