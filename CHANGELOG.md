# Changelog

## 0.1.0 (2026-06-18)


### ⚠ BREAKING CHANGES

* binary, command, env vars, and data dir are renamed.

### Features

* **cli:** S3 show/resume/name + session-grouped search ([38d4334](https://github.com/us/wend/commit/38d433409923d150eb28cc8bc71254e993a86b19))
* **cli:** show --range/--count + numbered messages + always show total ([9abc7ca](https://github.com/us/wend/commit/9abc7cae9842289b42d342266c2c68a21b270d56))
* **embed:** S7 semantic search (opt-in Candle, hybrid RRF) ([6d0699f](https://github.com/us/wend/commit/6d0699ff49bf2453485541b69020be75ab812374))
* **parser:** S1 jsonl parser — content flattening, routing, fixtures, golden tests ([510759e](https://github.com/us/wend/commit/510759e66454c6cbd5bc59397679a80118b7c2ff))
* **plugin:** S8 Claude Code plugin + README ([e9540e7](https://github.com/us/wend/commit/e9540e71a4237017113d4e8aaf64b3ad06be84d3))
* **recover:** S4 compaction recovery (the wedge) ([768974b](https://github.com/us/wend/commit/768974ba608174ce25217c3064be4bef7d6bc528))
* **release:** S6 cross-platform release pipeline ([539748e](https://github.com/us/wend/commit/539748e8fe51d373b87c86a6fe92cdd0e9d035d1))
* **store,index:** S2 SQLite index + FTS search ([39761f0](https://github.com/us/wend/commit/39761f0c90072155be0df82a082081fbebaa91a0))
* **topology:** S5 tree — worktree/session topology ([292bdb9](https://github.com/us/wend/commit/292bdb9661e9ac016ccd7a2f04f50d3b3bc4e116))


### Bug Fixes

* address 10-user QA findings ([df04d8f](https://github.com/us/wend/commit/df04d8f6f5c30c63d66861f9ab6c261ed4853370))
* **embed:** cap embedding threads so it doesn't pin the whole machine ([a59012a](https://github.com/us/wend/commit/a59012a8a39ddbce4543b14f59351c050a657612))
* **store:** make v3 migration idempotent for partially-present chunk tables ([977cce5](https://github.com/us/wend/commit/977cce58981088803bf4b10e1fca8ecdc040793a))


### Performance

* **embed:** chunk-level semantic via fastembed/ort — ~40h → ~10min ([ea661df](https://github.com/us/wend/commit/ea661dffc396588b7b47f96027694f4aa74d4397))


### Code Refactoring

* rename project recall -&gt; wend ([7abbea0](https://github.com/us/wend/commit/7abbea081f4808eaba465ba20a47aa3dceeb3af2))


### Miscellaneous

* release 0.1.0 ([9e42292](https://github.com/us/wend/commit/9e422929018b36112e798a4bbb44ec6d7ba0a629))
