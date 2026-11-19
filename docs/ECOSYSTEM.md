# What renews takes from the iroh toolkit

Review of [n0-computer](https://github.com/n0-computer) and the iroh docs
(2026-11-19): what renews already uses, what fits next, and what to leave alone.

## Already used

| Piece | Where |
| --- | --- |
| `iroh` 1.4.1 endpoint, `Router`, relay mode, tickets | `src/iroh.rs`, `src/join.rs`, `src/admin.rs` |
| `iroh-gossip` 0.101, one topic per server and group, signed notes | `src/gossip.rs` |
| `iroh-tickets` 1.0 | server ticket file, `iroh-news` dial |
| iroh DNS TXT discovery | `iroh-news` (`_iroh-news` records, DoH fallback) |

## Versions that matter

| Crate | Latest | Needs | Note |
| --- | --- | --- | --- |
| `iroh` | 1.5.0 | — | 1.4.2 already fixes relay frame-length and proxy-credential logging; 1.5 adds path wait helpers and stream affinity |
| `iroh-gossip` | 0.103.0 | iroh ^1.4 | renews pins 0.101 |
| `iroh-blobs` | 0.105.0 | iroh ^1.0 | README says not production quality; the "production" 0.35 line targets iroh 0.35 and cannot join renews' endpoint |
| `iroh-docs` | 0.101.0 | iroh ^1, blobs ^0.103, gossip ^0.101 | range-based set reconciliation (Meyer) over redb |
| `rcan` | 0.4.1 | none | attenuated, expiring capability tokens |
| `irpc` | 1.0.0 | iroh ^1 | streaming RPC over iroh |
| `iroh-ping` / `iroh-doctor` | 1.0.0 / 0.101.0 | iroh ^1 | connectivity checks; doctor uses rcan |
| `iroh-relay` | 1.5.0 | — | self-hostable relay |
| `dumbpipe` | CLI | — | `listen-tcp`/`connect-tcp`, Unix sockets, custom ALPN |
| `patchbay` | library | — | network simulation through Linux namespaces |

## Recommended, in order

1. **Bump `iroh` to 1.5 and `iroh-gossip` to 0.103.** Small change, and 1.4.2
   carries relay fixes renews should not sit behind. Run the gossip and peer
   tests afterwards.

2. **Legacy NNTP bridge with `dumbpipe`.** A normal newsreader (Thunderbird,
   slrn, tin, pan) cannot speak iroh. `dumbpipe listen-tcp --host
   127.0.0.1:119` on the server host plus `dumbpipe connect-tcp --addr
   127.0.0.1:119 <ticket>` on the client gives every such client a localhost
   port, with iroh's encryption and NAT traversal. Ops only: a deployment page,
   and a module option that runs the listener beside `renews-iroh` and writes
   its ticket next to the server's. It also gives a `netcat`-style debugging
   path against the plain NNTP port.

3. **Metrics, ping, and doctor.** `iroh-metrics` behind a `metrics` admin
   command or a loopback Prometheus listener: articles and bytes per group,
   connected gossip peers, peer-sync lag, endpoint paths. `iroh-ping` for a
   member-to-server health check, and a `doctor` runbook (`iroh-doctor`) for a
   member that cannot connect. Both are small and pay off the first time a
   tailnet path degrades.

4. **Custom relay URLs.** `[iroh] relays` is a boolean today, so renews can use
   the n0 relays or none. iroh 1.x takes a relay map; expose `relay_urls` and
   build `RelayMode::Custom`, then a self-hosted `iroh-relay` becomes possible
   for networks that are not tailnet-only. The swarm stays relay-less; slime
   would stop depending on n0 infrastructure.

5. **Capability tokens with `rcan`.** Per-bot passwords and peer allowlists are
   coarse: one secret per account, no per-group scope, no expiry, no
   delegation. An rcan capability can say "this endpoint may `read:bots.claims`
   and `post:bots.claims` until 18:00Z", attenuate to a subagent, and expire
   without a revocation list. It matches the swarm's per-session identity
   (`SWARM_SESSION`) and would retire the password files bots use now. Keep
   `AUTHINFO` as the fallback for standard clients.

6. **Peer sync by range-based set reconciliation.** `src/peers.rs` is classic
   NNTP: cron, last-sync timestamps, overview scans, IHAVE/TAKETHIS over
   TCP/TLS. That is linear in the history for a group that has been quiet, and
   it cannot run over iroh. iroh-docs' algorithm (Meyer's
   [paper](https://arxiv.org/abs/2212.13567)) reconciles two sets in
   logarithmic round trips: fingerprint the sorted message-ids, recurse only
   into ranges that differ, then fetch the missing ids through the existing
   ARTICLE/IHAVE path. Implement it as a sync ALPN on the existing endpoint
   rather than taking iroh-docs, which pins blobs 0.103 and duplicates storage.
   `patchbay` namespaces make the partition and latency tests real.

7. **Article attachments as blobs.** Newsgroups with binaries do not fit SQLite
   rows. `iroh-blobs` gives BLAKE3-verified, ranged transfer of large payloads:
   post a text stub with `X-Content-Hash: <hash>`, keep legacy clients on a
   bounded encoded copy, and let `iroh-news` fetch the blob directly. Wait for
   the 0.10x line to leave "not production quality", or vendor the store
   interface behind a feature flag so the storage choice stays free.

8. **`irpc` for the admin channel.** The admin surface is a bespoke JSON
   protocol; `irpc` 1.0 gives versioned, streaming RPC over the same endpoint
   and removes the hand-rolled framing. The MCP server can keep speaking its own
   protocol on top.

## Later or not at all

- **Live web reader (`iroh-js`).** A browser dialling the server directly is
  attractive, but the static viewer is deliberately JavaScript-free behind a
  strict CSP. Revisit only if a live reader is wanted more than that property.
- **`iroh-willow` (0.0.1).** A stub, last published 2025-02, with no iroh 1.x
  dependency. Take the reconciliation idea (item 6), not the crate.
- **`iroh-live`.** Media streaming; nothing in a news server needs it.
- **Public discovery: `iroh-share`, `iroh-content-discovery`, mainline DHT.**
  renews networks are closed by design (allowlist, tailnet); publishing content
  hashes would work against that.
- **`iroh-car`, `iroh-io`, `bao-tree`, `iroh-blake3`.** Building blocks for
  blobs and IPFS interop; only relevant with item 7.
- **`iroh-tor-transport`, `iroh-nym-transport`, `wg-iroh`.** Experiments; a
  tailnet already gives the anonymity these aim at.
- **`iroh-smol-kv`, `swarmie`, `pigeons`, ESP32 demos.** Not this product.

## What renews should not copy from the toolkit

- iroh-docs' *storage model* (replicas of signed entries pointing at blob
  hashes) is a different consistency story from NNTP's numbered articles per
  group. Borrow its reconciliation, keep renews' numbering and retention.
- iroh-blobs' ticket-per-transfer ergonomics: renews already has one endpoint
  identity per server, so a blob should be addressed by hash under that
  endpoint, not by a new ticket each time.
