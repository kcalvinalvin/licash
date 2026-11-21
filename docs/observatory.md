# Observatory reference

`licash-observatory` watches the hubs and swap providers of one network and
serves what it sees as a web page and a JSON API. Anyone can run one: the
page is read-only, and nothing on it can make the observatory act.
`licash-observatory run --help` lists every flag and environment variable.

```sh
licash-observatory --data ./observatory run --network signet --listen 0.0.0.0:8350
```

## Finding services

Every ten minutes, the observatory reads hub and provider announcements
(Nostr kind 38338) for its network from the usual relays, or from those
given with `--relay`. `--hub URL` and `--provider URL` add services that do
not announce themselves; such a service is known by its URL.

An announced service counts as up only if its info names the key that
announced it, so a URL cannot answer for another service. A service is
still probed for seven days after its last announcement, so going away
shows as downtime rather than vanishing. Probes are kept for 90 days.

## Probes, and Tor

Each service's URLs are probed every `--probe-every` seconds (300 by
default), each at a random moment in the first half of the round, so no
service can tell a probe from a user. Every probe goes over Tor, on its own
circuit, so every service is measured from the same place and none sees
the observatory's address. One probe makes three timings on one connection:

- **Setup**: the Tor circuit, the stream and TLS. This is mostly Tor's,
  and is shown apart.
- **Round trip**: `GET /health`. This is the service as seen through Tor,
  and leaderboards rank by its median.
- **Work**: `GET /v1/info` (or `/v1/swaps/info`) less the round trip, which
  is the time the service took to answer.

Onion URLs are reached the same way. `--direct` reaches services without
Tor, for regtest or a VPN of one's own: onion URLs are then skipped, and
services see the observatory's address.

## What the page shows

- **Overview**: services up, median latencies, leaderboards (uptime, round
  trip, success of measured moves) and the fees of each kind of payment on
  10,000 sat, at their cheapest and median.
- **Hubs** and **Swap providers**: each service's state, uptime, round
  trip and terms. *Into it* is what providers can move into a hub now, in
  all, which is the liquidity they offer it.
- **A service's page**: its URLs, a latency chart, its terms in full
  (Licash, Lightning, Bitcoin, protection and its interest), how far its
  chain is behind the other hubs', and the moves that used it.

Uptime is the share of probes that answered. A service with fewer than
three probes in the range is not ranked. The range is 24 hours, 7 days or
30 days (`?range=24h|7d|30d`).

Terms are read part by part. A service running another version, whose info
has a part this one cannot read, shows that part as not known, and is still
counted as up.

## JSON API

`GET /api/v1/view?range=7d` returns everything the pages show.
`GET /health` answers `ok`.

## Records

`observatory.sqlite` in the data directory holds the services, probes,
terms and moves. Its schema is versioned; an observatory refuses records
written by a later version rather than misreading them.

## Docker

```sh
docker build -f apps/observatory/Dockerfile -t licash-observatory .
docker run -d -p 8350:8350 -v licash-observatory:/data licash-observatory run --network signet
```

The image listens on `0.0.0.0:8350` and keeps its records and Tor's state
in `/data`.
