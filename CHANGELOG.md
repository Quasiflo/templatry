# Changelog

## [0.2.0](https://github.com/Quasiflo/templatry/compare/v0.1.0...v0.2.0) (2026-09-24)


### Features

* --no-color flag & env support ([8e2f915](https://github.com/Quasiflo/templatry/commit/8e2f9153b83cdcbc49d25970686d2e43dfef7cb6))

## 0.1.0 (2026-09-22)


### Features

* add `templatry cache clear` command ([1c73787](https://github.com/Quasiflo/templatry/commit/1c73787c650739d21e330ef2141b1d9daede07ce))
* backprop ignore lists with maintained state ([4c0ab8b](https://github.com/Quasiflo/templatry/commit/4c0ab8bea09109957f05d85067a0ef131a465681))
* explicit merge_json/yaml/toml strategies, remove bare merge ([aeda20b](https://github.com/Quasiflo/templatry/commit/aeda20b88ee97f7a2e633bb25ed12530392b9b5b))
* extends with abstract templates ([c91619c](https://github.com/Quasiflo/templatry/commit/c91619c9568344f9bbf92b36c784b36ec7c168fd))
* lenient JSON parsing with comment stripping ([225493d](https://github.com/Quasiflo/templatry/commit/225493dc6c2b7865e968deb53bbb040834b9d2e4))
* local_override_file third merge layer ([eff306b](https://github.com/Quasiflo/templatry/commit/eff306bb3744c92af4db2e2ec682f6451502e0b8))
* milestone 0 scaffolding (CLI shell, deps, deny, test harness) ([0370ae1](https://github.com/Quasiflo/templatry/commit/0370ae1b5085db6db78888a7912e90192b7a35d4))
* milestone 1 schemas and validate ([074a5cc](https://github.com/Quasiflo/templatry/commit/074a5ccbc20232eb61a016edbd7cdfdd3346e565))
* milestone 2 sources and cache ([2bc32d2](https://github.com/Quasiflo/templatry/commit/2bc32d2022636ad19705707e7fad18b4fd52ca14))
* milestone 3 merge, generate, and check ([a5af182](https://github.com/Quasiflo/templatry/commit/a5af182712475c931f8d6c02ccfb0e7cd0f30c9c))
* milestone 4 watch mode ([95d62a8](https://github.com/Quasiflo/templatry/commit/95d62a8576dab54fa36fea4b1ea5d07f6d2bb660))
* milestone 5 back-propagation ([54613c3](https://github.com/Quasiflo/templatry/commit/54613c3913baf5b8e57554f64c63463a209d06ea))
* multi-source project support ([source.&lt;name&gt;]) ([3578859](https://github.com/Quasiflo/templatry/commit/3578859ae8e05a0b6f9232c38656db3e2c624e20))
* none strategy and per-template project selection ([305fb4d](https://github.com/Quasiflo/templatry/commit/305fb4d15db4b35dbd13d3d936456378328e76cc))


### Bug Fixes

* allow dual-role repositories ([e8e1da7](https://github.com/Quasiflo/templatry/commit/e8e1da728671b02983ac549388c4336997998aee))
* backprop revert empties the override instead of failing ([d3bc836](https://github.com/Quasiflo/templatry/commit/d3bc83632ede24a24602253eea3cb6ed1b2bca61))
* collapse ambiguity on outcome-equivalent backprop candidates ([38cb58c](https://github.com/Quasiflo/templatry/commit/38cb58cc65b1ab02e61dd0bee6aa40a80c5817d8))
* copy permissions with files in none mode ([5ceb564](https://github.com/Quasiflo/templatry/commit/5ceb5640c40d59bfdd74f674308e818adcc2a71b))
* default to tag archive if github asset pattern isn't specified ([5bbbfa0](https://github.com/Quasiflo/templatry/commit/5bbbfa0b86ec191c9ea685ff5ed9f743d7dd9f14))
* **deps:** update cargo dependencies ([8d5dc6e](https://github.com/Quasiflo/templatry/commit/8d5dc6e80ffad6cc18a1e17359368cbf4727bee2))
* **deps:** update cargo dependencies ([#4](https://github.com/Quasiflo/templatry/issues/4)) ([1da0202](https://github.com/Quasiflo/templatry/commit/1da02025a45387651fa5eca0fbd654a58d76c635))
* **deps:** update cargo dependencies (major) ([#5](https://github.com/Quasiflo/templatry/issues/5)) ([8d5dc6e](https://github.com/Quasiflo/templatry/commit/8d5dc6e80ffad6cc18a1e17359368cbf4727bee2))
* union arrays when combining shared structured destinations ([b88201d](https://github.com/Quasiflo/templatry/commit/b88201dbfc3f5e7dfbe55f1df665b4b5296821c5))
