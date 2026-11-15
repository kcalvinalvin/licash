-- A wallet file as version 6 wrote it: wallet-v5.sql opened by version 6
-- (its migration to schema 6 applied): never rewrite it.
PRAGMA foreign_keys=OFF;
BEGIN TRANSACTION;
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
INSERT INTO meta VALUES(1,X'0707070707070707070707070707070707070707070707070707070707070707',X'0267c7f0dce3b320fdf96389e92400dd851b7c47260ba0faeb3c8b0c63298b0cac03b252d677791aa267ac25f601467018a007d15fe1af16add18408e71cb5cb7dfc020f611992bce27b529e413d73ca17d43242e0322594246a93f96e662a74eab592','regtest',1794660774);
CREATE TABLE hubs (
    id INTEGER PRIMARY KEY,
    info TEXT NOT NULL,
    updated_at INTEGER NOT NULL
, trusted INTEGER NOT NULL DEFAULT 1) STRICT;
INSERT INTO hubs VALUES(1,'{"keysets":["4c49434153482d4b5302431b887879f45258d8f5a856ce9823e320c94d565150946328de75f5814522d77ad3ff2b1066ffdfc65044f298eaa764b8316d29b1c64086dcb56eb824862862000000070000000000000001039c0ea7b901d67f00c718e4799a8e7e54b5a519cc95e86894c23ba7ef32e801700000000000000002036d9f3f70800687e6dfe95eac5e04b890154f2fef168ba010984b0ac0dec395300000000000000004033088f4dca2756f3168829c5561914b6fd85fe6e0310444e7ed3f3a33da1e9b51000000000000000803aa5878f3d5c50c4a0bb821207b9b236082f4523c7d682c5aa0980da01a8216e0000000000000001002ac4b6719ebe2bfbc6f3be12bcc674a217ada918f60797884cdffb4cf5af81d6d000000000000002003c622465d5941d2b07f92d43fe5654797ee6dc0c129cc00f7fd0462a4cc17a429000000000000004002ec4fa9b34ca2b57692e4cedc9428d71e61682398760940687d48dbc070cc8cf1"],"address":"0241070a91c26675b5d741bc44bbaa7a6708e970df7b203bfdeabbdbf77c2fcca0025d0a4cb6e8e33c6367a522e73c5121ce7d79f8305c1540bd2cf2711a60a152dd0362854ab4da6acc1d4056aee6739a294622e7a40171df7e3ead91fde49fd724a5","fees":{"spend":{"base":0,"ppm":0},"lock":{"base":0,"ppm":0},"refund":{"base":0,"ppm":0}},"locks":{"checkpoint":"4c49434153482d43500201000000000100000000000000000000000000000000000000000000000000000000000000000000003ba3edfd7a7b12b27ac72c3e67768f617fc81bc3888a51323a9fb8aa4b1e5e4adae5494dffff7f2002000000000000014d49e5da00","height":3,"window":20,"reaction":2,"min_depth":1,"max_headers":4032},"nostr":"05f3c458f8be422fdc9320b90322495c9ee6713543f6342aeb274d28fa5adb4a","tweaks":1}',1794660774,1);
CREATE TABLE hub_urls (
    url TEXT PRIMARY KEY,
    hub INTEGER NOT NULL REFERENCES hubs (id)
) STRICT;
INSERT INTO hub_urls VALUES('http://hub.test',1);
CREATE TABLE keysets (
    id BLOB PRIMARY KEY,
    hub INTEGER NOT NULL REFERENCES hubs (id),
    bytes BLOB NOT NULL
) STRICT;
INSERT INTO keysets VALUES(X'431b887879f45258d8f5a856ce9823e320c94d565150946328de75f5814522d7',1,X'4c49434153482d4b5302431b887879f45258d8f5a856ce9823e320c94d565150946328de75f5814522d77ad3ff2b1066ffdfc65044f298eaa764b8316d29b1c64086dcb56eb824862862000000070000000000000001039c0ea7b901d67f00c718e4799a8e7e54b5a519cc95e86894c23ba7ef32e801700000000000000002036d9f3f70800687e6dfe95eac5e04b890154f2fef168ba010984b0ac0dec395300000000000000004033088f4dca2756f3168829c5561914b6fd85fe6e0310444e7ed3f3a33da1e9b51000000000000000803aa5878f3d5c50c4a0bb821207b9b236082f4523c7d682c5aa0980da01a8216e0000000000000001002ac4b6719ebe2bfbc6f3be12bcc674a217ada918f60797884cdffb4cf5af81d6d000000000000002003c622465d5941d2b07f92d43fe5654797ee6dc0c129cc00f7fd0462a4cc17a429000000000000004002ec4fa9b34ca2b57692e4cedc9428d71e61682398760940687d48dbc070cc8cf1');
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
CREATE TABLE receipts (
    id BLOB PRIMARY KEY,
    keyset BLOB NOT NULL REFERENCES keysets (id),
    amount INTEGER NOT NULL,
    received_at INTEGER NOT NULL
) STRICT;
CREATE TABLE silent_scan (
    one INTEGER PRIMARY KEY CHECK (one = 1),
    next INTEGER NOT NULL
, untweaked INTEGER) STRICT;
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
CREATE TABLE silent_spends (
    txid BLOB PRIMARY KEY,
    tx BLOB NOT NULL,
    created_at INTEGER NOT NULL
) STRICT;
CREATE TABLE silent_fallback (
    one INTEGER PRIMARY KEY CHECK (one = 1),
    shown INTEGER NOT NULL
) STRICT;
INSERT INTO silent_fallback VALUES(1,0);
CREATE TABLE payouts (
    hash BLOB PRIMARY KEY,
    provider TEXT NOT NULL,
    preimage BLOB NOT NULL,
    claim_key BLOB NOT NULL,
    from_hub INTEGER NOT NULL REFERENCES hubs (id),
    address TEXT NOT NULL,
    script BLOB NOT NULL,
    amount INTEGER NOT NULL,
    budget INTEGER NOT NULL,
    fee INTEGER NOT NULL,
    refund_key BLOB,
    timeout INTEGER,
    funding_txid BLOB,
    funding_vout INTEGER,
    funding_height INTEGER,
    value INTEGER,
    claim_tx BLOB,
    revealed_at INTEGER,
    scanned INTEGER,
    spent_height INTEGER,
    created_at INTEGER NOT NULL,
    done_at INTEGER,
    failed TEXT
) STRICT;
CREATE VIEW reach (hub, url) AS
    SELECT hub, url FROM hub_urls
    WHERE rowid IN (SELECT MIN(rowid) FROM hub_urls GROUP BY hub);
CREATE INDEX hub_urls_hub ON hub_urls (hub);
CREATE INDEX notes_spent_by ON notes (spent_by);
COMMIT;
PRAGMA user_version=6;
