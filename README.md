# cordis-rust

[Cordis](https://github.com/cordiverse/cordis) 的 Rust 实现——时空可组合
（spatiotemporal composability）插件框架，概念蓝本为论文
[arXiv:2608.25512](https://arxiv.org/abs/2608.25512) 及其参考实现 cordis v4。

```rust
use cordis::prelude::*;

let ctx = Context::new();

// 运行时动态安装；插件体注册的一切随卸载逆序回滚
let db = ctx.plugin(DatabasePlugin, serde_json::json!({ "uri": "pg://localhost" }))?;

// 声明依赖的插件被响应式驱动：服务出现即激活，消失即停用
let app = ctx.plugin(AppPlugin, serde_json::json!(null))?;
db.dispose()?;                    // app -> Pending，副作用完全回滚
ctx.plugin(DatabasePlugin, ...)?; // app -> Active，无需重启进程
```

核心能力：

- **可逆效应**（时间性可组合）：`ctx.effect` 收集清理函数，卸载严格逆序执行；
  事件监听、服务注册、子插件全部构建于同一机制之上。
- **响应式依赖**（空间性可组合）：`Inject::required/optional` 声明余效应，
  提供者装卸自动驱动依赖方激活/停用（epoch 签名机制）。
- **五模式事件的同步子集**：`emit` / `bail` / `waterfall`（middleware 链），
  字符串名 + serde 类型化双通道，`internal/*` 生命周期事件。
- **同步优先**：无异步运行时依赖，FFI 回调可安全重入（快照后调用）。
- **二进制插件分发**：插件以预编译 cdylib 分发，C ABI（JSON 字节 + 句柄 +
  回调蹦床）与宿主 rustc/依赖版本解耦；运行时安装、热更新配置、卸载回滚，
  全程不重启。

| crate | 状态 | 说明 |
|---|---|---|
| `cordis` | ✅ M1 | 核心：Context / Fiber / Effect / 事件 / 服务 |
| `cordis-plugin-api` | ✅ M2 | 插件侧稳定 C ABI：Plugin trait + `register_plugin!` |
| `cordis-loader` | ✅ M2 | 运行时安装 cdylib 二进制插件（文件 / cargo 构建 / URL 下载） |
| `cordis-timer` | ✅ M3 | 后台定时轮：setTimeout/setInterval/sleep，随作用域自动取消 |
| `cordis-logger` | ✅ M3 | 分级日志服务 + 可插拔 sink，按插件 scope 归属 |
| macros / 异步生命周期 / 插件仓库 | 规划中 | 见 `docs/design.md` §5 路线图 |

设计细节与 TS 版分歧清单见 [docs/design.md](docs/design.md)。
开发约定见 [AGENTS.md](AGENTS.md)。

```sh
cargo test                          # 验收测试
cargo run -p cordis --example lifecycle
```
