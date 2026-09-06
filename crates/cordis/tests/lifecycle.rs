//! Acceptance tests for the cordis core: dynamic install/uninstall,
//! revertible effects, reactive dependency resolution, events.

use std::sync::{Arc, Mutex};

use cordis::prelude::*;
use serde_json::{Value, json};

/// Shared append-only log for asserting setup/dispose ordering.
type Log = Arc<Mutex<Vec<String>>>;

fn log_fn(log: &Log, msg: &str) -> impl FnOnce() + Send + 'static {
    let log = log.clone();
    let msg = msg.to_owned();
    move || log.lock().unwrap().push(msg.clone())
}

fn push(log: &Log, msg: &str) {
    log.lock().unwrap().push(msg.to_owned());
}

/// A disposer that records its execution (Box coercion needs the target type).
fn dispose_log(log: &Log, msg: &str) -> Disposer {
    Box::new(log_fn(log, msg))
}

#[allow(dead_code)]
fn _keep_log_fn(f: impl FnOnce() + Send + 'static) -> Disposer {
    Box::new(f)
}

fn logged(log: &Log) -> Vec<String> {
    log.lock().unwrap().clone()
}

// ---------------------------------------------------------------------
// 1. Dynamic install/uninstall with full reverse-order reversion
// ---------------------------------------------------------------------

#[test]
fn dynamic_install_uninstall_reverts_in_reverse_order() {
    let ctx = Context::new();
    let log: Log = Arc::default();

    let plugin = {
        let log = log.clone();
        move |ctx: &Context, _config: &Value| -> PluginResult {
            let log = log.clone();
            ctx.effect("effect/a", {
                let log = log.clone();
                move |_| {
                    push(&log, "a:setup");
                    let log = log.clone();
                    Ok(Some(dispose_log(&log, "a:dispose")))
                }
            })?;
            ctx.effect("effect/b", {
                let log = log.clone();
                move |_| {
                    push(&log, "b:setup");
                    let log = log.clone();
                    Ok(Some(dispose_log(&log, "b:dispose")))
                }
            })?;
            Ok(())
        }
    };

    let handle = ctx.plugin(plugin, json!(null)).expect("install");
    assert!(handle.is_active());
    assert_eq!(logged(&log), ["a:setup", "b:setup"]);

    assert!(handle.dispose());
    assert_eq!(
        logged(&log),
        ["a:setup", "b:setup", "b:dispose", "a:dispose"]
    );
    assert_eq!(handle.state(), FiberState::Disposed);

    // Disposing twice is a no-op.
    assert!(!handle.dispose());
    assert_eq!(logged(&log).len(), 4);
}

// ---------------------------------------------------------------------
// 2. Nested effects are claimed by their enclosing effect
// ---------------------------------------------------------------------

#[test]
fn nested_effects_cascade_to_outer_dispose() {
    let ctx = Context::new();
    let log: Log = Arc::default();

    let outer = {
        let log = log.clone();
        ctx.effect("outer", move |ctx: &Context| {
            let inner_log = log.clone();
            ctx.effect("inner", move |_| {
                push(&inner_log, "inner:setup");
                Ok(Some(dispose_log(&inner_log, "inner:dispose")))
            })?;
            push(&log, "outer:setup");
            Ok(Some(dispose_log(&log, "outer:dispose")))
        })
        .expect("outer effect")
    };

    // Disposing the outer effect reverts the inner one too (v4 claim
    // semantics), inner first.
    outer.dispose();
    assert_eq!(
        logged(&log),
        [
            "inner:setup",
            "outer:setup",
            "outer:dispose",
            "inner:dispose"
        ]
    );
    assert!(!outer.is_alive());
}

// ---------------------------------------------------------------------
// 3. Reactive dependency resolution: pending -> active -> deactivate
//    on provider loss, reactivate on provider return
// ---------------------------------------------------------------------

#[test]
fn dependents_follow_their_provider_through_install_cycles() {
    let ctx = Context::new();
    let log: Log = Arc::default();

    struct Database {
        #[allow(dead_code)]
        uri: String,
    }

    let provider = {
        let log = log.clone();
        move |ctx: &Context, config: &Value| -> PluginResult {
            let uri = config["uri"].as_str().unwrap_or_default().to_owned();
            let log = log.clone();
            ctx.effect("provider/tracker", move |_| {
                push(&log, "provider:live");
                let log = log.clone();
                Ok(Some(dispose_log(&log, "provider:gone")))
            })?;
            ctx.provide("database", Arc::new(Database { uri }))?;
            Ok(())
        }
    };

    // A consumer that *declares* its dependency (reactive coeffect).
    struct ConsumerPlugin(Log);
    impl Plugin for ConsumerPlugin {
        fn name(&self) -> Option<&str> {
            Some("consumer")
        }
        fn inject(&self) -> Inject {
            Inject::required(&["database"])
        }
        fn apply(&self, ctx: &Context, _config: &Value) -> PluginResult {
            let db = ctx
                .get::<Database>("database")
                .expect("required dependency visible");
            let log = self.0.clone();
            let uri = db.uri.clone();
            ctx.effect("consumer/tracker", move |_| {
                push(&log, &format!("consumer:live({uri})"));
                Ok(Some(dispose_log(&log, "consumer:halted")))
            })?;
            Ok(())
        }
    }

    // Consumer installed first: stays pending, body unexecuted.
    let consumer_fiber = ctx
        .plugin_dyn(Arc::new(ConsumerPlugin(log.clone())), json!(null))
        .expect("install consumer");
    assert_eq!(consumer_fiber.state(), FiberState::Pending);
    assert!(logged(&log).is_empty());

    // Provider arrives: the consumer activates reactively.
    let provider_arc = Arc::new(provider.clone()) as Arc<dyn Plugin>;
    let provider_fiber = ctx
        .plugin_dyn(provider_arc, json!({"uri": "pg://x"}))
        .expect("install provider");
    assert!(provider_fiber.is_active());
    assert!(consumer_fiber.is_active());
    assert_eq!(logged(&log), ["provider:live", "consumer:live(pg://x)"]);

    // Provider leaves: the consumer deactivates, its effects revert.
    provider_fiber.dispose();
    assert_eq!(consumer_fiber.state(), FiberState::Pending);
    assert_eq!(
        logged(&log),
        [
            "provider:live",
            "consumer:live(pg://x)",
            "consumer:halted", // dependent halted inside provider's withdrawal
            "provider:gone",
        ]
    );

    // Provider returns (fresh install): the consumer reloads.
    let again = ctx
        .plugin_dyn(
            Arc::new(provider.clone()) as Arc<dyn Plugin>,
            json!({"uri": "pg://y"}),
        )
        .expect("reinstall provider");
    assert!(consumer_fiber.is_active());
    assert!(logged(&log).contains(&"consumer:live(pg://y)".to_owned()));

    again.dispose();
    assert_eq!(consumer_fiber.state(), FiberState::Pending);
}

// ---------------------------------------------------------------------
// 4. Optional dependencies never gate activation
// ---------------------------------------------------------------------

#[test]
fn optional_dependencies_do_not_block() {
    let ctx = Context::new();

    struct Assets;
    let with_optional = |ctx: &Context, _config: &Value| -> PluginResult {
        assert!(!ctx.has("assets"), "optional service absent at activation");
        ctx.effect("probe", |_| Ok(()))?;
        Ok(())
    };
    let handle = ctx.plugin(with_optional, json!(null)).expect("install");
    assert!(handle.is_active());

    // An optional provider appearing later does not reload the fiber but
    // becomes visible through the global store.
    let provider = ctx
        .plugin(
            |ctx: &Context, _cfg: &Value| -> PluginResult {
                ctx.provide("assets", Arc::new(Assets))?;
                Ok(())
            },
            json!(null),
        )
        .expect("install assets");
    assert!(ctx.has("assets"));
    provider.dispose();
    assert!(!ctx.has("assets"));
    assert!(handle.is_active(), "optional loss must not deactivate");
}

// ---------------------------------------------------------------------
// 5. Typed/raw service access and set() rules
// ---------------------------------------------------------------------

#[test]
fn service_access_channels_and_set_rules() {
    let ctx = Context::new();

    ctx.provide("root-svc", Arc::new(42_u32))
        .expect("root provide");
    assert_eq!(ctx.get::<u32>("root-svc").map(|v| *v), Some(42));
    assert_eq!(ctx.get::<String>("root-svc"), None, "type mismatch -> None");
    assert!(ctx.has("root-svc"));

    // set() requires a prior provide on the same fiber.
    assert!(matches!(
        ctx.set("missing", Arc::new(1_u32)),
        Err(CordisError::NotProvided { .. })
    ));
    ctx.set("root-svc", Arc::new(43_u32))
        .expect("set own service");
    assert_eq!(ctx.get::<u32>("root-svc").map(|v| *v), Some(43));

    // set() from another fiber is rejected.
    let other = ctx
        .plugin(
            |ctx: &Context, _cfg: &Value| -> PluginResult {
                let result = ctx.set("root-svc", Arc::new(0_u32));
                assert!(matches!(result, Err(CordisError::ForeignService { .. })));
                Ok(())
            },
            json!(null),
        )
        .expect("install");
    assert!(other.is_active());

    // Duplicate provide is a conflict.
    let dupe = ctx
        .plugin(
            |ctx: &Context, _cfg: &Value| -> PluginResult {
                ctx.provide_raw("root-svc", Arc::new(1_u32))?;
                Ok(())
            },
            json!(null),
        )
        .expect("install");
    assert_eq!(dupe.state(), FiberState::Failed);
    assert!(dupe.error().is_some());
}

// ---------------------------------------------------------------------
// 6. Event modes
// ---------------------------------------------------------------------

#[test]
fn event_modes_emit_once_bail_waterfall_and_removal() {
    let ctx = Context::new();
    let log: Log = Arc::default();

    let h1 = ctx
        .on("tick", {
            let log = log.clone();
            move |_, args| {
                push(&log, &format!("h1:{}", args["n"]));
                None
            }
        })
        .expect("on");
    ctx.on_prepend("tick", {
        let log = log.clone();
        move |_, _| {
            push(&log, "h0");
            None
        }
    })
    .expect("prepend");
    ctx.emit("tick", &json!({"n": 1}));
    assert_eq!(logged(&log), ["h0", "h1:1"]);

    // Removing via handle.
    h1.dispose();
    log.lock().unwrap().clear();
    ctx.emit("tick", &json!({"n": 2}));
    assert_eq!(logged(&log), ["h0"]);

    // once fires exactly once.
    log.lock().unwrap().clear();
    ctx.once("flash", {
        let log = log.clone();
        move |_, _| {
            push(&log, "flash");
            None
        }
    })
    .expect("once");
    ctx.emit("flash", &Value::Null);
    ctx.emit("flash", &Value::Null);
    assert_eq!(logged(&log), ["flash"]);

    // bail: first Some short-circuits.
    ctx.on("pick", |_, _| Some(json!(1))).expect("on");
    ctx.on("pick", |_, _| Some(json!(2))).expect("on");
    assert_eq!(ctx.bail("pick", &Value::Null), Some(json!(1)));

    // waterfall: middlewares run in registration order, fallback terminates.
    ctx.middleware("pipeline", |_, mut value, next| {
        value["extra"] = json!(true);
        next(value)
    })
    .expect("mw");
    ctx.middleware("pipeline", |_, value, next| {
        next(json!({ "wrapped": value }))
    })
    .expect("mw2");
    let out = ctx
        .waterfall("pipeline", json!({"core": 1}), |v| json!({"final": v}))
        .expect("waterfall");
    assert_eq!(
        out,
        json!({"final": {"wrapped": {"core": 1, "extra": true}}})
    );
}

#[test]
fn registering_on_a_disposed_fiber_fails() {
    let ctx = Context::new();
    let slot: Arc<Mutex<Option<Context>>> = Arc::default();
    let slot2 = slot.clone();

    let fiber = ctx
        .plugin(
            move |ctx: &Context, _cfg: &Value| -> PluginResult {
                *slot2.lock().unwrap() = Some(ctx.clone());
                Ok(())
            },
            json!(null),
        )
        .expect("install");
    fiber.dispose();

    let stale_ctx = slot.lock().unwrap().clone().expect("context captured");
    assert!(matches!(
        stale_ctx.on("x", |_, _| None),
        Err(CordisError::InactiveContext { .. })
    ));
    assert!(matches!(
        stale_ctx.effect("x", |_| Ok(())),
        Err(CordisError::InactiveContext { .. })
    ));
    assert!(matches!(
        stale_ctx.plugin(
            |_: &Context, _: &Value| -> PluginResult { Ok(()) },
            json!(null)
        ),
        Err(CordisError::InactiveContext { .. })
    ));
}

// ---------------------------------------------------------------------
// 7. update() restart and Failed recovery
// ---------------------------------------------------------------------

#[test]
fn update_restarts_and_clears_failures() {
    let ctx = Context::new();
    let log: Log = Arc::default();

    struct Broker {
        #[allow(dead_code)]
        url: String,
    }
    // Fails unless config has a "url".
    let broker = {
        let log = log.clone();
        move |ctx: &Context, config: &Value| -> PluginResult {
            let url = config["url"].as_str().ok_or_else(|| {
                PluginError::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "missing url",
                ))
            })?;
            let log = log.clone();
            let url_for_effect = url.to_owned();
            ctx.effect("broker", move |_| {
                push(&log, &format!("broker:live({url_for_effect})"));
                Ok(Some(dispose_log(&log, "broker:down")))
            })?;
            ctx.provide(
                "broker",
                Arc::new(Broker {
                    url: url.to_owned(),
                }),
            )?;
            Ok(())
        }
    };

    let fiber = ctx
        .plugin(broker, json!({"nope": true}))
        .expect("install returns handle");
    assert_eq!(fiber.state(), FiberState::Failed);
    assert!(fiber.error().is_some());
    // Failed body effects were rolled back.
    assert!(logged(&log).is_empty());
    assert!(!ctx.has("broker"));

    // update() clears the failure and restarts.
    fiber.update(json!({"url": "amqp://q"})).expect("update");
    assert!(fiber.is_active());
    assert_eq!(logged(&log), ["broker:live(amqp://q)"]);
    assert!(ctx.has("broker"));

    // update() with a bad config fails again; previous effects revert.
    assert!(fiber.update(json!({})).is_err());
    assert_eq!(fiber.state(), FiberState::Failed);
    assert_eq!(logged(&log), ["broker:live(amqp://q)", "broker:down"]);
    assert!(!ctx.has("broker"));

    // restart() reports the failure until update clears it.
    assert!(fiber.restart().is_err());
    fiber.update(json!({"url": "amqp://q2"})).expect("recover");
    assert_eq!(
        logged(&log),
        [
            "broker:live(amqp://q)",
            "broker:down",
            "broker:live(amqp://q2)"
        ]
    );
    assert_eq!(fiber.config().expect("config"), json!({"url": "amqp://q2"}));
}

// ---------------------------------------------------------------------
// 8. Multiple instances of one plugin are independent
// ---------------------------------------------------------------------

#[test]
fn multiple_instances_are_independent() {
    let ctx = Context::new();

    struct Worker {
        #[allow(dead_code)]
        id: u32,
    }
    let worker = |ctx: &Context, config: &Value| -> PluginResult {
        ctx.provide(
            "worker",
            Arc::new(Worker {
                id: config["id"].as_u64().unwrap_or(0) as u32,
            }),
        )?;
        Ok(())
    };

    let a = ctx.plugin(worker, json!({"id": 1})).expect("a");
    // Same plugin logic, fresh install.
    let worker2 = |ctx: &Context, config: &Value| -> PluginResult {
        ctx.provide(
            "worker2",
            Arc::new(Worker {
                id: config["id"].as_u64().unwrap_or(0) as u32,
            }),
        )?;
        Ok(())
    };
    let _ = ctx.plugin(worker2, json!({"id": 2})).expect("b");

    assert!(a.is_active());
    a.dispose();
    assert_eq!(a.state(), FiberState::Disposed);
    assert!(ctx.has("worker2"), "other instance survives");
}

// ---------------------------------------------------------------------
// 9. ctx.inject() sugar with dependency gating
// ---------------------------------------------------------------------

#[test]
fn inject_sugar_gates_on_dependencies() {
    let ctx = Context::new();

    let gate = Arc::new(Mutex::new(false));
    let gate2 = gate.clone();
    let handle = ctx
        .inject(Inject::required(&["gate"]), move |ctx: &Context| {
            assert!(ctx.get::<bool>("gate").is_some());
            *gate2.lock().unwrap() = true;
            Ok(())
        })
        .expect("install");
    assert_eq!(handle.state(), FiberState::Pending);
    assert!(!*gate.lock().unwrap());

    ctx.provide("gate", Arc::new(true)).expect("provide gate");
    assert!(handle.is_active());
    assert!(*gate.lock().unwrap());
}

// ---------------------------------------------------------------------
// 10. Re-entrancy: plugins install/plugins and listeners mutate state
// ---------------------------------------------------------------------

#[test]
fn reentrant_registration_from_listeners_and_bodies() {
    let ctx = Context::new();

    // A listener that installs a plugin, which provides a service and
    // registers another listener, which provides another service.
    let installer = ctx
        .on("boot", |ctx: &Context, _| {
            let inner = ctx
                .plugin(
                    |ctx: &Context, _cfg: &Value| -> PluginResult {
                        ctx.provide("inner-svc", Arc::new("inner".to_owned()))?;
                        ctx.on("inner-evt", |_, _| Some(json!("inner-listener")))?;
                        Ok(())
                    },
                    json!(null),
                )
                .expect("inner install");
            assert!(inner.is_active());
            None
        })
        .expect("on boot");

    ctx.emit("boot", &Value::Null);
    assert!(ctx.has("inner-svc"));
    assert_eq!(
        ctx.bail("inner-evt", &Value::Null),
        Some(json!("inner-listener"))
    );

    // Dispose the listener handle: the installed plugin keeps living
    // (it was a normal install, not an effect of the listener).
    installer.dispose();
    ctx.emit("boot", &Value::Null);
    assert!(ctx.has("inner-svc"));

    // A plugin body installing a child plugin: child dies with parent.
    let child_gone: Arc<Mutex<bool>> = Arc::default();
    let child_gone2 = child_gone.clone();
    let parent = ctx
        .plugin(
            move |ctx: &Context, _cfg: &Value| -> PluginResult {
                let gone = child_gone2.clone();
                ctx.plugin(
                    move |ctx: &Context, _cfg: &Value| -> PluginResult {
                        let gone = gone.clone();
                        ctx.effect("child-effect", move |_| {
                            let d: Disposer = Box::new(move || {
                                *gone.lock().unwrap() = true;
                            });
                            Ok(Some(d))
                        })?;
                        Ok(())
                    },
                    json!(null),
                )?;
                Ok(())
            },
            json!(null),
        )
        .expect("parent");

    parent.dispose();
    assert!(
        *child_gone.lock().unwrap(),
        "child effect reverted with parent"
    );
    let _ = installer;
}

// ---------------------------------------------------------------------
// 11. internal/* lifecycle events
// ---------------------------------------------------------------------

#[test]
fn internal_events_report_lifecycle() {
    let ctx = Context::new();
    let log: Log = Arc::default();

    ctx.on("internal/status", {
        let log = log.clone();
        move |_, args| {
            let arr = args.as_array().expect("array");
            let summary = &arr[0];
            push(
                &log,
                &format!(
                    "status:{}:{}->{}",
                    summary["name"].as_str().unwrap_or("?"),
                    arr[1].as_str().unwrap_or("?"),
                    summary["state"].as_str().unwrap_or("?")
                ),
            );
            None
        }
    })
    .expect("on status");
    ctx.on("internal/service", {
        let log = log.clone();
        move |_, args| {
            let arr = args.as_array().expect("array");
            push(&log, &format!("service:{}", arr[0].as_str().unwrap_or("?")));
            None
        }
    })
    .expect("on service");

    struct S;
    let svc = ctx
        .plugin(
            |ctx: &Context, _cfg: &Value| -> PluginResult {
                ctx.provide("svc", Arc::new(S))?;
                Ok(())
            },
            json!(null),
        )
        .expect("install svc");
    svc.dispose();

    // internal/status carries the fiber summary plus the OLD state
    // (cordis v4 parity): pending -> active at activation, and
    // active -> disposed at teardown.
    let events = logged(&log);
    assert!(
        events.contains(&"status:anonymous:pending->active".to_owned()),
        "{events:?}"
    );
    assert_eq!(
        events.iter().filter(|e| *e == "service:svc").count(),
        2,
        "service announced on provide and on withdrawal: {events:?}"
    );
    assert!(
        events.contains(&"status:anonymous:active->disposed".to_owned()),
        "{events:?}"
    );
}

// ---------------------------------------------------------------------
// 12. Panicking plugins fail their fiber instead of the host
// ---------------------------------------------------------------------

#[test]
fn panicking_plugin_becomes_failed_fiber() {
    let ctx = Context::new();

    let fiber = ctx
        .plugin(
            |ctx: &Context, _cfg: &Value| -> PluginResult {
                ctx.effect("halfway", |_| Ok(()))?; // must be rolled back
                panic!("boom");
            },
            json!(null),
        )
        .expect("install returns handle");
    assert_eq!(fiber.state(), FiberState::Failed);
    assert!(fiber.error().expect("error message").contains("boom"));

    // The runtime stays healthy: new plugins install fine.
    let ok = ctx
        .plugin(
            |_: &Context, _: &Value| -> PluginResult { Ok(()) },
            json!(null),
        )
        .expect("install after panic");
    assert!(ok.is_active());
}

// ---------------------------------------------------------------------
// 13. Typed events
// ---------------------------------------------------------------------

#[test]
fn typed_events_round_trip() {
    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize, Debug, PartialEq)]
    struct Ping {
        seq: u32,
    }

    struct PingEvent;
    impl Event for PingEvent {
        const NAME: &'static str = "net/ping";
        type Args = Ping;
        type Ret = u32;
    }

    let ctx = Context::new();
    ctx.on_event::<PingEvent>(|_, args| {
        if args.seq >= 3 {
            return Some(args.seq * 10);
        }
        None
    })
    .expect("on_event");

    // Below threshold: no bail value; payload round-trips.
    assert_eq!(ctx.bail_event::<PingEvent>(&Ping { seq: 1 }), None);
    assert_eq!(ctx.bail_event::<PingEvent>(&Ping { seq: 3 }), Some(30));

    // Typed emit dispatches on the string key (observable via bail).
    ctx.emit_event::<PingEvent>(&Ping { seq: 4 });
}
