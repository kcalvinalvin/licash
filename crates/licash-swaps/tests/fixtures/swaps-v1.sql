-- A swaps file as version 1 wrote it (schema 1, one swap of notes accepted
-- and not locked yet, its event): never rewrite it.
PRAGMA foreign_keys=OFF;
BEGIN TRANSACTION;
CREATE TABLE meta (
    one INTEGER PRIMARY KEY CHECK (one = 1),
    -- The secret key the provider announces itself under on Nostr.
    nostr BLOB NOT NULL,
    created_at INTEGER NOT NULL
) STRICT;
INSERT INTO meta VALUES(1,X'1111111111111111111111111111111111111111111111111111111111111111',1763164800);
CREATE TABLE swaps (
    id BLOB PRIMARY KEY,
    hash BLOB NOT NULL UNIQUE,
    from_url TEXT NOT NULL,
    to_url TEXT NOT NULL,
    address BLOB NOT NULL,
    amount INTEGER NOT NULL,
    fee INTEGER NOT NULL,
    paid INTEGER NOT NULL,
    expiry_from INTEGER NOT NULL,
    expiry INTEGER NOT NULL,
    lock BLOB,
    preimage BLOB,
    state TEXT NOT NULL,
    reason TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
) STRICT;
INSERT INTO swaps VALUES(X'abababababababababababababababababababababababababababababababab',X'cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd','http://a.test','http://b.test',X'020202020202020202020202020202020202020202020202020202020202020202',1000,11,1011,200,194,NULL,NULL,'accepted',NULL,1763164800,1763164800);
CREATE TABLE events (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    at INTEGER NOT NULL,
    kind TEXT NOT NULL,
    json TEXT NOT NULL
) STRICT;
INSERT INTO events VALUES(1,1763164800,'accepted','{"kind":"accepted","id":"abababababababababababababababababababababababababababababababab","amount":1000,"fee":11,"from":"http://a.test","to":"http://b.test"}');
CREATE INDEX swaps_state ON swaps (state);
COMMIT;
PRAGMA user_version=1;
