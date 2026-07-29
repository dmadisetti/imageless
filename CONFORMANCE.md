# Conformance evidence

SPEC.md §7 defines conformance; this file records who has demonstrated it,
against what, and when. The v1 freeze criteria (ROADMAP.md) require every row
marked **freeze** to hold evidence, not intent — this standard exists so that
evidence is recorded as it happens instead of reconstructed later.

## Evidence standard

A **gate run** entry is filled only by a link to a CI run (or the exact
command plus commit for a local run) naming the consumer under test, the
gate, the compatibility-matrix cell (containerd / Docker / runc versions),
the imageless commit, and the date.

A **deployment attestation** is filled only with operator, date, the spec
sections exercised, and the configuration surface actually used:

- external references (§3): the policy prefixes allow-listed and the pinned
  form of the deployed reference (`?rev=` / `narHash`);
- release profile (§6): catalog kind (directory or HTTPS), the substituters
  and whether signatures were verified (public keys configured), and the
  store projection mode.

"Exercised by a real deployment" (freeze criterion 2) means a workload
someone ran for its own sake. A gate run does not qualify, however realistic.

## Gate runs

The matrix cell is a single column until the compatibility matrix lands in
CI (ROADMAP v0.2); after that, one row per consumer x gate x cell.

| Consumer | Gate | Matrix cell | Commit | Date | Evidence | Freeze |
|---|---|---|---|---|---|---|
| imageless-runc | docker-embedded-smoke | containerd 2.3.1 / Docker 29.5.2 / runc 1.4.2 | 3abc69f (merged as 1335c5b) | 2026-07-29 | <https://github.com/dmadisetti/imageless/actions/runs/30428954833> | required |
| imageless-runc | imageless-cri-vm | containerd 2.3.1 / Docker 29.5.2 / runc 1.4.2 | 3abc69f (merged as 1335c5b) | 2026-07-29 | <https://github.com/dmadisetti/imageless/actions/runs/30428954833> | required |
| cowboy-runtime | docker-embedded-smoke | — | — | — | pending (needs the consumer-pluggable gate harness) | required |
| cowboy-runtime | imageless-cri-vm | — | — | — | pending (needs the consumer-pluggable gate harness) | required |

## Deployment attestations

| Spec section | Operator | Date | Configuration | Freeze |
|---|---|---|---|---|
| §3 external references | — | — | — | required |
| §6 release profile | — | — | — | required |

Known gap to close before the §6 row can count: the release gate today uses a
local directory catalog, a `file:///nix/store` substituter, and no signing
keys. Freeze evidence needs at least one run or deployment with an HTTPS
issuer catalog and a substituter fetch verified against configured public
keys.
