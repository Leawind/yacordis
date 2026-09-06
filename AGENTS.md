# Agent 指南

## 最高约束

最高约束由用户（人类开发者）亲自编写，Agent 不得擅动。

- 本仓库用于实现 [Cordis](https://github.com/cordiverse/cordis) 的 Rust 版本。

## 其他

- 提交信息（commit message）一律用英文，Conventional Commits 风格（feat/fix/docs/test...）
- AGENTS-LOCAL.md 中存放本地开发环境下所用工具链相关的经验，建议阅读

## 踩坑记录

实现中踩过并确认的坑，供后续任务避让；条目应写清现象与结论，不罗列排查过程。

- Rust coherence：为本地 trait（如 `Plugin`）写 blanket `impl<T: Plugin> From<T> for Arc<dyn Plugin>` 会与 std 的自反 `impl<T> From<T> for T` 冲突（E0119），因为下游 crate 可以为 `Arc<dyn Plugin>` 实现本地 trait。结论：不要给 foreign 类型做 blanket From/Into 转换；改为提供两个显式入口（`plugin(impl Plugin)` 与 `plugin_dyn(Arc<dyn Plugin>)`）。
- 自定义错误 `PluginError` 若实现 `std::error::Error`，则 blanket `impl<E: Error> From<E> for PluginError`（为让插件体内 `?` 可用）在 E=PluginError 时同样自反冲突。结论：让 `PluginError` 不实现 `Error`（仅实现 Display），blanket From 即可共存；错误链经内部的 `Box<dyn Error>` 的 `source()` 走。
- `Box::new(closure) as Box<dyn FnOnce>` 不合法（E0605，trait object 强转不允许 `as`）；`Box::new(closure)` 也要在目标类型已知处才发生 unsize coercion。结论：给辅助函数标注返回类型（如 `fn dispose_log(...) -> Disposable`），在函数体内完成装箱。
- 包裹用户代码的 `catch_unwind` 中，被调方若持有「链的续体」（如 waterfall 的 `next: FnOnce`），panic unwind 会把它连同尚未使用的参数一起丢弃，Err 分支无法重放。结论：调用前必须克隆需要保住的值（如 `Value::clone`），panic 视为弃权并用克隆值继续。
- 「插件失败即回滚」要做成显式路径：v4 TS 在插件体抛错后把已收集的 disposer 留到下次卸载才执行；Rust 版选择失败立即逆序回滚，保证 `Failed` 态零副作用。后续实现新的失败路径时不得沿用 v4 的残留语义。
- 同 fiber 的生命周期操作可能被级联（notify → 依赖方 reload）重入；任何在锁内调用用户代码的写法都会死锁或破坏快照语义。结论：全库坚持「快照后调用」+ per-fiber `busy` 标志与 pending 队列，新增 API 时同样不得在持锁状态触发监听器/disposer/插件体。
- FFI 边界缓冲区的所有权规则必须**只有一条**：cordis-loader 首版同时存在「被调方释放输入」（宿主 vtable 的 take_string/take_json）与「调用方释放输入」（插件侧 Context 调完后 free）两套逻辑，导致同一缓冲区释放两次，glibc 报 `double free or corruption (fasttop)` 直接 abort。结论：定死「被调方释放输入缓冲区；调用方读取后释放输出缓冲区」，两侧代码逐一审计；且缓冲区统一走进程共享的 `malloc`/`free`（同进程只有一个 libc 实例），比「各侧释放各自分配」省掉整层 free 函数配对。排查此类问题直接 gdb bt，栈顶就是第二处 free。
- edition 2024 中 `#[no_mangle]` 属于 unsafe attribute，宏里必须写 `#[unsafe(no_mangle)]`；由于宏的属性检查跟随**宏定义 crate** 的 edition，即便展开进 edition 2021 的插件 crate 也必须用 unsafe(...) 形式。
- `cargo build --message-format=json` 时，rustc 的编译诊断以 `reason: "compiler-message"` JSON 行走 stdout（人类可读文本在 `message.rendered`），stderr 只剩 cargo 自身错误。加载器报告构建失败时必须合并两处输出，否则只见 "could not compile" 看不到原因。
- `CARGO_NET_OFFLINE` 环境变量只接受字面量 `true`/`false`（CLI flag 才接受 `--offline`）；传 `1` 会让 cargo 直接报错。
- 同一 fiber 的生命周期调度若用「原子 busy 标志 + 独立的 pending 锁」配对，多线程下存在丢唤醒窗口（racer 在持锁者最后排空与 busy 释放之间入队的 op 永不被处理）。结论：busy 与队列收进单个 Mutex，push 与 busy 检查共享锁，用户代码在锁外执行；并发正确性不靠原子序数推理，靠锁的临界区保证。
- 跨 FFI 回调记录的宿主侧包装类型，`Drop` 必须是唯一释放路径；再写显式释放调用（哪怕是错误清理路径）就会双重释放。需要「提前放弃」语义时用 `mem::replace` 置空后再Drop，共享 `user_data` 的 `Clone` 直接禁止。
- 测试诊断日志若在【持锁 A 时取锁 B】而另一路径【持 B 取 A】，会把竞态 bug 变成必现 ABBA 死锁——本轮 trace 环形缓冲在 `check_impls`（持 fiber.inner）里重取 inner 就是这么挂的。结论：诊断代码自身必须比被诊断代码「锁更少」，读状态放在取诊断锁之前；且 thread_local 计数器手动增减时必须配对 panic 安全（Drop guard），否则 unwind 后计数泄漏误判重入。
- 「wait until counter==0」式的全局 quiesce 若被【op 执行路径内的重入调用】（插件 body 内 install/dispose 触发的级联）命中，会等待自己正在执行的外层 op，必然自死锁。结论：提供同步语义的等待原语时必须先检测「本线程是否已在 op 内」（thread-local 深度计数），是则跳过，由外层 op 的完成承担语义。
