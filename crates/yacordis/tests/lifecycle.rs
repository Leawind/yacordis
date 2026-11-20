use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use yacordis::prelude::*;

type Log = Arc<Mutex<Vec<String>>>;

fn log_fn(log: &Log, msg: &str) -> impl FnOnce() + Send + 'static {
    let log = log.clone();
    let msg = msg.to_owned();
    move || log.lock().unwrap().push(msg.clone())
}

fn push(log: &Log, msg: &str) {
    log.lock().unwrap().push(msg.to_owned());
}

/// Box coercion needs the target type known.
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

#[test]
fn dynamic_install_uninstall_reverts_in_reverse_order() {
    let ctx = Context::new();
    let log: Log = Arc::default();

    let component = {
        let log = log.clone();
        move |ctx: &Context, _config: &Value| -> ComponentResult {
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

    let handle = ctx.component(component, json!(null)).expect("install");
    assert!(handle.is_active());
    assert_eq!(logged(&log), ["a:setup", "b:setup"]);

    assert!(handle.dispose());
    assert_eq!(
        logged(&log),
        ["a:setup", "b:setup", "b:dispose", "a:dispose"]
    );
    assert_eq!(handle.state(), FiberState::Disposed);

    assert!(!handle.dispose());
    assert_eq!(logged(&log).len(), 4);
}

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

    // v4 claim semantics: the enclosing effect's dispose reverts the inner one too.
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
        move |ctx: &Context, config: &Value| -> ComponentResult {
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

    struct ConsumerComponent(Log);
    impl Component for ConsumerComponent {
        fn name(&self) -> Option<&str> {
            Some("consumer")
        }
        fn inject(&self) -> Inject {
            Inject::required(&["database"])
        }
        fn apply(&self, ctx: &Context, _config: &Value) -> ComponentResult {
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

    let consumer_fiber = ctx
        .component_dyn(Arc::new(ConsumerComponent(log.clone())), json!(null))
        .expect("install consumer");
    assert_eq!(consumer_fiber.state(), FiberState::Pending);
    assert!(logged(&log).is_empty());

    let provider_arc = Arc::new(provider.clone()) as Arc<dyn Component>;
    let provider_fiber = ctx
        .component_dyn(provider_arc, json!({"uri": "pg://x"}))
        .expect("install provider");
    assert!(provider_fiber.is_active());
    assert!(consumer_fiber.is_active());
    assert_eq!(logged(&log), ["provider:live", "consumer:live(pg://x)"]);

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

    let again = ctx
        .component_dyn(
            Arc::new(provider.clone()) as Arc<dyn Component>,
            json!({"uri": "pg://y"}),
        )
        .expect("reinstall provider");
    assert!(consumer_fiber.is_active());
    assert!(logged(&log).contains(&"consumer:live(pg://y)".to_owned()));

    again.dispose();
    assert_eq!(consumer_fiber.state(), FiberState::Pending);
}

#[test]
fn optional_dependencies_do_not_block() {
    let ctx = Context::new();

    struct Assets;
    let with_optional = |ctx: &Context, _config: &Value| -> ComponentResult {
        assert!(!ctx.has("assets"), "optional service absent at activation");
        ctx.effect("probe", |_| Ok(()))?;
        Ok(())
    };
    let handle = ctx.component(with_optional, json!(null)).expect("install");
    assert!(handle.is_active());

    let provider = ctx
        .component(
            |ctx: &Context, _cfg: &Value| -> ComponentResult {
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

#[test]
fn service_access_channels_and_set_rules() {
    let ctx = Context::new();

    ctx.provide("root-svc", Arc::new(42_u32))
        .expect("root provide");
    assert_eq!(ctx.get::<u32>("root-svc").map(|v| *v), Some(42));
    assert_eq!(ctx.get::<String>("root-svc"), None, "type mismatch -> None");
    assert!(ctx.has("root-svc"));

    assert!(matches!(
        ctx.set("missing", Arc::new(1_u32)),
        Err(CordisError::NotProvided { .. })
    ));
    ctx.set("root-svc", Arc::new(43_u32))
        .expect("set own service");
    assert_eq!(ctx.get::<u32>("root-svc").map(|v| *v), Some(43));

    let other = ctx
        .component(
            |ctx: &Context, _cfg: &Value| -> ComponentResult {
                let result = ctx.set("root-svc", Arc::new(0_u32));
                assert!(matches!(result, Err(CordisError::ForeignService { .. })));
                Ok(())
            },
            json!(null),
        )
        .expect("install");
    assert!(other.is_active());

    let dupe = ctx
        .component(
            |ctx: &Context, _cfg: &Value| -> ComponentResult {
                ctx.provide_raw("root-svc", Arc::new(1_u32))?;
                Ok(())
            },
            json!(null),
        )
        .expect("install");
    assert_eq!(dupe.state(), FiberState::Failed);
    assert!(dupe.error().is_some());
}

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

    h1.dispose();
    log.lock().unwrap().clear();
    ctx.emit("tick", &json!({"n": 2}));
    assert_eq!(logged(&log), ["h0"]);

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

    ctx.on("pick", |_, _| Some(json!(1))).expect("on");
    ctx.on("pick", |_, _| Some(json!(2))).expect("on");
    assert_eq!(ctx.bail("pick", &Value::Null), Some(json!(1)));

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
        .component(
            move |ctx: &Context, _cfg: &Value| -> ComponentResult {
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
        stale_ctx.component(
            |_: &Context, _: &Value| -> ComponentResult { Ok(()) },
            json!(null)
        ),
        Err(CordisError::InactiveContext { .. })
    ));
}

#[test]
fn update_restarts_and_clears_failures() {
    let ctx = Context::new();
    let log: Log = Arc::default();

    struct Broker {
        #[allow(dead_code)]
        url: String,
    }
    let broker = {
        let log = log.clone();
        move |ctx: &Context, config: &Value| -> ComponentResult {
            let url = config["url"].as_str().ok_or_else(|| {
                ComponentError::new(std::io::Error::new(
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
        .component(broker, json!({"nope": true}))
        .expect("install returns handle");
    assert_eq!(fiber.state(), FiberState::Failed);
    assert!(fiber.error().is_some());
    assert!(logged(&log).is_empty());
    assert!(!ctx.has("broker"));

    fiber.update(json!({"url": "amqp://q"})).expect("update");
    assert!(fiber.is_active());
    assert_eq!(logged(&log), ["broker:live(amqp://q)"]);
    assert!(ctx.has("broker"));

    assert!(fiber.update(json!({})).is_err());
    assert_eq!(fiber.state(), FiberState::Failed);
    assert_eq!(logged(&log), ["broker:live(amqp://q)", "broker:down"]);
    assert!(!ctx.has("broker"));

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

#[test]
fn multiple_instances_are_independent() {
    let ctx = Context::new();

    struct Worker {
        #[allow(dead_code)]
        id: u32,
    }
    let worker = |ctx: &Context, config: &Value| -> ComponentResult {
        ctx.provide(
            "worker",
            Arc::new(Worker {
                id: config["id"].as_u64().unwrap_or(0) as u32,
            }),
        )?;
        Ok(())
    };

    let a = ctx.component(worker, json!({"id": 1})).expect("a");
    let worker2 = |ctx: &Context, config: &Value| -> ComponentResult {
        ctx.provide(
            "worker2",
            Arc::new(Worker {
                id: config["id"].as_u64().unwrap_or(0) as u32,
            }),
        )?;
        Ok(())
    };
    let _ = ctx.component(worker2, json!({"id": 2})).expect("b");

    assert!(a.is_active());
    a.dispose();
    assert_eq!(a.state(), FiberState::Disposed);
    assert!(ctx.has("worker2"), "other instance survives");
}

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

#[test]
fn reentrant_registration_from_listeners_and_bodies() {
    let ctx = Context::new();

    let installer = ctx
        .on("boot", |ctx: &Context, _| {
            let inner = ctx
                .component(
                    |ctx: &Context, _cfg: &Value| -> ComponentResult {
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

    // The installed component keeps living: a normal install, not an effect of the listener.
    installer.dispose();
    ctx.emit("boot", &Value::Null);
    assert!(ctx.has("inner-svc"));

    let child_gone: Arc<Mutex<bool>> = Arc::default();
    let child_gone2 = child_gone.clone();
    let parent = ctx
        .component(
            move |ctx: &Context, _cfg: &Value| -> ComponentResult {
                let gone = child_gone2.clone();
                ctx.component(
                    move |ctx: &Context, _cfg: &Value| -> ComponentResult {
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
        .component(
            |ctx: &Context, _cfg: &Value| -> ComponentResult {
                ctx.provide("svc", Arc::new(S))?;
                Ok(())
            },
            json!(null),
        )
        .expect("install svc");
    svc.dispose();

    // internal/status carries the OLD state, not the new one (cordis v4 parity).
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

#[test]
fn panicking_component_becomes_failed_fiber() {
    let ctx = Context::new();

    let fiber = ctx
        .component(
            |ctx: &Context, _cfg: &Value| -> ComponentResult {
                ctx.effect("halfway", |_| Ok(()))?; // must be rolled back
                panic!("boom");
            },
            json!(null),
        )
        .expect("install returns handle");
    assert_eq!(fiber.state(), FiberState::Failed);
    assert!(fiber.error().expect("error message").contains("boom"));

    let ok = ctx
        .component(
            |_: &Context, _: &Value| -> ComponentResult { Ok(()) },
            json!(null),
        )
        .expect("install after panic");
    assert!(ok.is_active());
}

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

    assert_eq!(ctx.bail_event::<PingEvent>(&Ping { seq: 1 }), None);
    assert_eq!(ctx.bail_event::<PingEvent>(&Ping { seq: 3 }), Some(30));

    // emit dispatches on the string key; observable only via a listener.
    ctx.emit_event::<PingEvent>(&Ping { seq: 4 });
}

/// An effect registered outside the body's scope (an event listener firing
/// after the body completed) lands in a later batch on the same fiber.
/// Disposal must follow global reverse-registration order across batches —
/// TS keeps one reverse-ordered disposables list; our batches concatenate
/// to it.
#[test]
fn disposal_is_reverse_registration_across_effect_batches() {
    let ctx = Context::new();
    let log: Log = Arc::default();

    let component = {
        let log = log.clone();
        move |ctx: &Context, _config: &Value| -> ComponentResult {
            let own = ctx.clone();
            ctx.effect("body/a", {
                let log = log.clone();
                move |_| {
                    push(&log, "a:setup");
                    Ok(Some(dispose_log(&log, "a:dispose")))
                }
            })?;
            // Fires after the body completes: `later/b` forms a second batch
            // on this fiber.
            own.on("later", {
                let own = own.clone();
                let log = log.clone();
                move |_, _| {
                    own.effect("later/b", {
                        let log = log.clone();
                        move |_| {
                            push(&log, "b:setup");
                            Ok(Some(dispose_log(&log, "b:dispose")))
                        }
                    })
                    .expect("register b on the live fiber");
                    None
                }
            })?;
            Ok(())
        }
    };

    let handle = ctx.component(component, json!(null)).expect("install");
    assert!(handle.is_active());

    ctx.emit("later", &json!(null));
    assert_eq!(logged(&log), ["a:setup", "b:setup"]);

    handle.dispose();
    assert_eq!(
        logged(&log),
        ["a:setup", "b:setup", "b:dispose", "a:dispose"],
        "global reverse-registration disposal order across batches"
    );
}

#[test]
fn required_dependency_gates_on_provider_activation() {
    let ctx = Context::new();
    ctx.set_logger(Arc::new(DiscardLogger));

    // Observations from the child body: one entry per body run, `true`
    // when the required service resolved.
    let runs: Arc<Mutex<Vec<bool>>> = Arc::default();
    let child_state_at_install: Arc<Mutex<Option<FiberState>>> = Arc::default();

    let runs_for_body = runs.clone();
    let child_state = child_state_at_install.clone();
    let parent = ctx
        .component(
            move |ctx: &Context, _: &Value| -> ComponentResult {
                ctx.provide("svc", Arc::new(json!({ "v": 1 })))?;
                // The provider is mid-body (still Pending): a required
                // dependency must not count as satisfied yet.
                let runs_inner = runs_for_body.clone();
                let child = ctx.inject(Inject::required(&["svc"]), move |ctx| {
                    runs_inner
                        .lock()
                        .unwrap()
                        .push(ctx.get::<Value>("svc").is_some());
                    Ok(())
                })?;
                *child_state.lock().unwrap() = Some(child.state());
                Ok(())
            },
            json!(null),
        )
        .expect("install parent");

    assert!(parent.is_active());
    assert_eq!(
        *child_state_at_install.lock().unwrap(),
        Some(FiberState::Pending),
        "a mid-body provider does not satisfy required injects"
    );
    // The child ran exactly once — after the provider activated — and its
    // required dependency resolved there.
    assert_eq!(
        *runs.lock().unwrap(),
        vec![true],
        "child body runs only on provider activation, with the service resolvable"
    );
}

#[test]
fn dispose_all_reverts_in_reverse_installation_order() {
    let ctx = Context::new();
    ctx.set_logger(Arc::new(DiscardLogger));
    let log: Log = Arc::default();

    let make = |tag: &'static str| -> Arc<dyn Component> {
        let log = log.clone();
        Arc::new(move |ctx: &Context, _: &Value| -> ComponentResult {
            let log = log.clone();
            ctx.effect(tag, move |_| Ok(Some(dispose_log(&log, tag))))?;
            Ok(())
        })
    };

    let first = ctx
        .component_dyn(make("first"), json!(null))
        .expect("first");
    let second = ctx
        .component_dyn(make("second"), json!(null))
        .expect("second");
    assert!(first.is_active() && second.is_active());

    ctx.dispose_all();
    assert_eq!(
        logged(&log),
        ["second", "first"],
        "later installations revert first across fibers"
    );
}
