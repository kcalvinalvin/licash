-- A wallet file as version 2 wrote it (schema 2, a hub added, watching for
-- silent payments from block 1, a coin found and spent): never rewrite it.
PRAGMA foreign_keys=OFF;
BEGIN TRANSACTION;
CREATE TABLE deposits (
    hash BLOB PRIMARY KEY,
    hub INTEGER NOT NULL REFERENCES hubs (id),
    preimage BLOB NOT NULL,
    refund_key BLOB NOT NULL,
    amount INTEGER NOT NULL,
    body TEXT NOT NULL,
    claim BLOB,
    timeout INTEGER,
    pays INTEGER,
    address TEXT,
    fund_by INTEGER,
    txid BLOB,
    vout INTEGER,
    height INTEGER,
    lock BLOB,
    refund BLOB,
    created_at INTEGER NOT NULL,
    done_at INTEGER,
    canceled TEXT
) STRICT;
CREATE TABLE hub_urls (
    url TEXT PRIMARY KEY,
    hub INTEGER NOT NULL REFERENCES hubs (id)
) STRICT;
INSERT INTO hub_urls VALUES('http://hub.test',1);
CREATE TABLE hubs (
    id INTEGER PRIMARY KEY,
    info TEXT NOT NULL,
    updated_at INTEGER NOT NULL
) STRICT;
INSERT INTO hubs VALUES(1,'{"keysets":["4c49434153482d4b530265fc281072b6cd7c36f646074c821e7e1994d6613c6b29a245a14841b8b840b1306c67fb44aa82b5c4d23af433eb2e0ba672f7688fc47fb92eea5ea6096758140000000700000000000000010392559f1af56b9900eb0896413e96b7ea75d251c68101c13eaaf5c2bdc5195ed3000000000000000202aec00199d7d72b1f9c19fab46eb777545c7dfb84a9b5ed782a350540f65eb7b70000000000000004021b77313095afea6bb0677bc945e54d19de7d95b4c8c8be584267cc7142a1de39000000000000000803447433d7a74f5191a88a6856d382b5219d4332707690cd2b7a6d52faefbd5e9600000000000000100308ea9e8865a1a7d23352ad550effb70284e21f53a43257cf7fce81028282ff8f0000000000000020038705c2130ddb009dd647ef8e0c59b2318a563312b2d2ad8f23726f7a355280e1000000000000004002c5eb5f14be0789456523c6fcdbcdc453af22bd9b919b151e9a3fd555ca3b9f94"],"address":"03060cc4708394675dc3078cd1d6ce6fbf783019e0d4879326af5b4887baf1eb7403e16f3a30c31f779ba758e832a602a7aaa32cbef3bdee70fda741f9e27006919502ecd02d013d9a5234cfab6bab4df6df73cd962d2fa8aa0d7ca0301b0b243495b8","fees":{"spend":{"base":0,"ppm":0},"lock":{"base":0,"ppm":0},"refund":{"base":0,"ppm":0}},"locks":{"checkpoint":"4c49434153482d43500201000000000100000000000000000000000000000000000000000000000000000000000000000000003ba3edfd7a7b12b27ac72c3e67768f617fc81bc3888a51323a9fb8aa4b1e5e4adae5494dffff7f2002000000000000014d49e5da00","height":3,"window":20,"reaction":2,"min_depth":1,"max_headers":4032},"nostr":"dbdf47a236f1f3020dfdfb07c3a2038725c3ab3f1a55211b4b5e92a3410b165a","tweaks":1}',1794643942);
CREATE TABLE invoices (
    hash BLOB PRIMARY KEY,
    hub INTEGER NOT NULL REFERENCES hubs (id),
    preimage BLOB NOT NULL,
    amount INTEGER NOT NULL,
    body TEXT NOT NULL,
    request TEXT,
    lock BLOB,
    created_at INTEGER NOT NULL,
    done_at INTEGER,
    canceled TEXT
) STRICT;
CREATE TABLE keysets (
    id BLOB PRIMARY KEY,
    hub INTEGER NOT NULL REFERENCES hubs (id),
    bytes BLOB NOT NULL
) STRICT;
INSERT INTO keysets VALUES(X'65fc281072b6cd7c36f646074c821e7e1994d6613c6b29a245a14841b8b840b1',1,X'4c49434153482d4b530265fc281072b6cd7c36f646074c821e7e1994d6613c6b29a245a14841b8b840b1306c67fb44aa82b5c4d23af433eb2e0ba672f7688fc47fb92eea5ea6096758140000000700000000000000010392559f1af56b9900eb0896413e96b7ea75d251c68101c13eaaf5c2bdc5195ed3000000000000000202aec00199d7d72b1f9c19fab46eb777545c7dfb84a9b5ed782a350540f65eb7b70000000000000004021b77313095afea6bb0677bc945e54d19de7d95b4c8c8be584267cc7142a1de39000000000000000803447433d7a74f5191a88a6856d382b5219d4332707690cd2b7a6d52faefbd5e9600000000000000100308ea9e8865a1a7d23352ad550effb70284e21f53a43257cf7fce81028282ff8f0000000000000020038705c2130ddb009dd647ef8e0c59b2318a563312b2d2ad8f23726f7a355280e1000000000000004002c5eb5f14be0789456523c6fcdbcdc453af22bd9b919b151e9a3fd555ca3b9f94');
CREATE TABLE locks (
    id BLOB PRIMARY KEY,
    hub INTEGER NOT NULL REFERENCES hubs (id),
    payee BLOB NOT NULL,
    hash BLOB NOT NULL,
    expiry INTEGER NOT NULL,
    amount INTEGER NOT NULL,
    fee INTEGER NOT NULL,
    request TEXT NOT NULL,
    certificate BLOB,
    -- 'pay' or 'refund'.
    settled TEXT,
    preimage BLOB,
    invoice TEXT,
    failure TEXT,
    created_at INTEGER NOT NULL,
    done_at INTEGER,
    abandoned TEXT
) STRICT;
CREATE TABLE meta (
    one INTEGER PRIMARY KEY CHECK (one = 1),
    -- The owner's seed, unless the application keeps it elsewhere.
    seed BLOB,
    -- The owner's address, which the seed opening the wallet must derive.
    address BLOB NOT NULL,
    -- The network of the hubs it trusts, by name: bitcoin, regtest, signet
    -- or mutinynet.
    network TEXT NOT NULL,
    created_at INTEGER NOT NULL
) STRICT;
INSERT INTO meta VALUES(1,X'0707070707070707070707070707070707070707070707070707070707070707',X'0267c7f0dce3b320fdf96389e92400dd851b7c47260ba0faeb3c8b0c63298b0cac03b252d677791aa267ac25f601467018a007d15fe1af16add18408e71cb5cb7dfc020f611992bce27b529e413d73ca17d43242e0322594246a93f96e662a74eab592','regtest',1794643942);
CREATE TABLE notes (
    y BLOB PRIMARY KEY,
    keyset BLOB NOT NULL REFERENCES keysets (id),
    denom INTEGER NOT NULL,
    note BLOB NOT NULL,
    proposal BLOB NOT NULL,
    ephemeral BLOB NOT NULL,
    branch INTEGER NOT NULL,
    position INTEGER NOT NULL,
    -- The send or lock that spends it, from when it chose it.
    spent_by BLOB,
    -- 1 unlinked, 0 linked, -1 linked for good: its hub unlinks it no more.
    unlinked INTEGER NOT NULL DEFAULT 0,
    -- An unlink under way: its session, to ask the hub again.
    session TEXT,
    -- When its hub showed it spent by a payment this file never made, by a
    -- copy of the wallet: it is never spent here.
    gone_at INTEGER,
    added_at INTEGER NOT NULL
) STRICT;
CREATE TABLE receipts (
    id BLOB PRIMARY KEY,
    keyset BLOB NOT NULL REFERENCES keysets (id),
    amount INTEGER NOT NULL,
    received_at INTEGER NOT NULL
) STRICT;
CREATE TABLE sends (
    id BLOB PRIMARY KEY,
    hub INTEGER NOT NULL REFERENCES hubs (id),
    payee BLOB NOT NULL,
    amount INTEGER NOT NULL,
    fee INTEGER NOT NULL,
    offer TEXT NOT NULL,
    release TEXT,
    unlock BLOB,
    created_at INTEGER NOT NULL,
    done_at INTEGER,
    abandoned TEXT
) STRICT;
CREATE TABLE swaps (
    hash BLOB PRIMARY KEY,
    provider TEXT NOT NULL,
    preimage BLOB NOT NULL,
    from_hub INTEGER NOT NULL REFERENCES hubs (id),
    to_hub INTEGER NOT NULL REFERENCES hubs (id),
    amount INTEGER NOT NULL,
    fee INTEGER NOT NULL,
    theirs BLOB,
    created_at INTEGER NOT NULL,
    done_at INTEGER,
    failed TEXT
) STRICT;
CREATE TABLE silent_scan (
    one INTEGER PRIMARY KEY CHECK (one = 1),
    next INTEGER NOT NULL
) STRICT;
INSERT INTO silent_scan VALUES(1,1);
CREATE TABLE silent_blocks (
    height INTEGER PRIMARY KEY,
    hash BLOB NOT NULL
) STRICT;
CREATE TABLE silent_coins (
    txid BLOB NOT NULL,
    vout INTEGER NOT NULL,
    value INTEGER NOT NULL,
    output BLOB NOT NULL,
    tweak BLOB NOT NULL,
    label INTEGER,
    height INTEGER NOT NULL,
    spent_by BLOB,
    spent_at INTEGER,
    PRIMARY KEY (txid, vout)
) STRICT;
INSERT INTO silent_coins VALUES(X'2222222222222222222222222222222222222222222222222222222222222222',0,10000,X'79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798',X'0101010101010101010101010101010101010101010101010101010101010101',NULL,1,X'3333333333333333333333333333333333333333333333333333333333333333',1);
CREATE TABLE silent_spends (
    txid BLOB PRIMARY KEY,
    tx BLOB NOT NULL,
    created_at INTEGER NOT NULL
) STRICT;
CREATE VIEW reach (hub, url) AS
    SELECT hub, url FROM hub_urls
    WHERE rowid IN (SELECT MIN(rowid) FROM hub_urls GROUP BY hub);
CREATE INDEX hub_urls_hub ON hub_urls (hub);
CREATE INDEX notes_spent_by ON notes (spent_by);
COMMIT;
PRAGMA user_version = 2;
