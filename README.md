# deepseek-rs

Single-binary Rust port of the Python `aiodeepseek` client + `deepseek_proxy`.

The Python implementation in `../deepseek` stays authoritative until parity is
proven; this repo is the port target.

## Status

| Crate | What | State |
|---|---|---|
| `pow` | `DeepSeekHashV1` proof-of-work | done, verified vs live server |
| `protocol` | DeepSeek SSE fragment router -> OpenAI chunks | in progress |
| `proxy` | axum HTTP shell (`/v1/chat/completions`) | not started |

## `DeepSeekHashV1`

SHA3-256 with one mutation: Keccak-f[1600] **skips round 0** (runs rounds
1..=23). No off-the-shelf SHA3 crate works; the permutation is implemented
here. Round constants beyond `RC[0]` are standard Keccak values.

See `crates/pow/src/lib.rs` and `crates/protocol/tests/corpus.rs`.

## Layout

```
crates/pow       DeepSeekHashV1 + multithreaded nonce search
crates/protocol  SSE parsing + fragment routing + OpenAI chunk shaping
crates/proxy     axum server, config, storage
corpus/          real captured SSE streams used as test fixtures
```

## Corpus

`corpus/*.sse` are raw byte streams captured from
`POST https://chat.deepseek.com/api/v0/chat/completion`. `corpus/*.expected.json`
holds the routing output produced by the Python implementation; the Rust tests
assert byte-for-byte equality against them.

Regenerate expectations with:

```bash
../deepseek/.venv/bin/python tools/gen_expected.py
```

## Build / test

```bash
cargo build --release
cargo test
```
