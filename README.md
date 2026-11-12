# cordis-rust

[Cordis](https://github.com/cordiverse/cordis) 的 Rust 实现——时空可组合
（spatiotemporal composability）组件框架，概念蓝本为论文
[arXiv:2608.25512](https://arxiv.org/abs/2608.25512) 及其参考实现 cordis v4。

```rust
use cordis::prelude::*;

let ctx = Context::new();

// 运行时动态安装；组件体注册的一切随卸载逆序回滚
let db = ctx.component(DatabaseComponent, serde_json::json!({ "uri": "pg://localhost" }))?;

// 声明依赖的组件被响应式驱动：服务出现即激活，消失即停用
let app = ctx.component(AppComponent, serde_json::json!(null))?;
db.dispose();                     // app -> Pending，副作用完全回滚
ctx.component(DatabaseComponent, ...)?; // app -> Active，进程内热生效
```

核心能力：

- **可逆效应**（时间性可组合）：`ctx.effect` 收集清理函数，卸载严格逆序执行；
  事件监听、服务注册、子组件全部构建于同一机制之上。
- **响应式依赖**（空间性可组合）：`Inject::required/optional` 声明余效应，
  提供者装卸自动驱动依赖方激活/停用（epoch 签名机制）；依赖环经
  `Component::provides` 声明检测并报告（对应 TS 版悬而未决的 issue #66）。
- **五模式事件的同步子集**：`emit` / `bail` / `waterfall`（middleware 链），
  字符串名 + serde 类型化双通道，`internal/*` 生命周期事件。
- **同步优先**：无异步运行时依赖，FFI 回调可安全重入（快照后调用）。
  epoch ABA、provider/consumer 卸载顺序等竞态窗口在同步核中
  **结构性不存在**，且每一项免疫都有回归测试锁定。
- **二进制组件分发**：组件以预编译 cdylib 分发，C ABI（JSON 字节 + 句柄 +
  回调蹦床）与宿主 rustc/依赖版本解耦；运行时安装、热更新配置、卸载回滚，
  全程零重启，作为一等能力实现（约 1,400 行 FFI 边界）。

| crate | 状态 | 说明 |
|---|---|---|
| `cordis` | ✅ M1 | 核心：Context / Fiber / Effect / 事件 / 服务 / isolate / 依赖环诊断 |
| `cordis-component-api` | ✅ M2 | 组件侧稳定 C ABI（v3）：Component trait + `register_component!` + `run_blocking` |
| `cordis-loader` | ✅ M2 | 运行时安装 cdylib 二进制组件（文件 / cargo 构建 / URL 下载）+ 仓库索引 + 卸载契约 lint |
| `cordis-timer` | ✅ M3 | 后台定时轮：setTimeout/setInterval/sleep，随作用域自动取消 |
| `cordis-logger` | ✅ M3 | 分级日志服务 + 可插拔 sink，按组件 scope 归属 |
| `cordis-macros` | ✅ | `#[component]` 属性宏：普通函数 + 声明 → Component 适配器 |

核心能力之外的里程碑全部落地：框架中介并发（`run_blocking`/
`spawn_future`，排空先于回滚）、`replace_component` 原位热替换、配置
schema 校验（resolveConfig 等价默认值）、组件仓库索引（semver +
sha-256）、卸载限制契约机械化（ast-grep lint + dispose 审计）——
见 `docs/design.md` §6。范式验证见孤儿分支 `orphan/dsh-demo`
（dsh-like agent harness，工作树 `../dsh-demo`）。

设计细节与 TS 版分歧清单、上游 issue 对照见
[docs/design.md](docs/design.md)（§3、§7）。
开发约定见 [AGENTS.md](AGENTS.md)。

```sh
cargo test                          # 验收测试
cargo run -p cordis --example lifecycle
```
