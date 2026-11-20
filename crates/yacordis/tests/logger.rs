use std::sync::Arc;

use serde_json::{Value, json};
use yacordis::MemorySink;
use yacordis::prelude::*;

#[test]
fn records_flow_to_sinks_with_component_scopes() {
    let ctx = Context::new();
    ctx.set_logger(Arc::new(yacordis::DiscardLogger));
    ctx.component(LoggerComponent, json!(null))
        .expect("install logger");

    let memory = Arc::new(MemorySink::new());
    let service = ctx.logger().expect("logger service");
    service.add_arc_sink(memory.clone());
    service.set_level(Some(LogLevel::Debug));

    let emitter = ctx
        .component(
            |ctx: &Context, _: &Value| -> ComponentResult {
                ctx.log_info("hello from emitter");
                ctx.log_debug("verbose detail");
                Ok(())
            },
            json!(null),
        )
        .expect("install emitter");

    let records = memory.records();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].level, LogLevel::Info);
    assert_eq!(records[0].scope, "anonymous");
    assert_eq!(records[0].message, "hello from emitter");
    assert_eq!(records[1].level, LogLevel::Debug);
    assert_eq!(records[1].message, "verbose detail");
    emitter.dispose();
}

#[test]
fn level_threshold_filters_records() {
    let ctx = Context::new();
    ctx.set_logger(Arc::new(yacordis::DiscardLogger));
    ctx.component(LoggerComponent, json!(null))
        .expect("install logger");

    let memory = Arc::new(MemorySink::new());
    let service = ctx.logger().expect("logger service");
    service.add_arc_sink(memory.clone());

    ctx.log_debug("hidden by default");
    assert_eq!(memory.records().len(), 0);

    ctx.log_warn("visible");
    assert_eq!(memory.records().len(), 1);

    ctx.log_error("boom");
    let records = memory.records();
    assert_eq!(records[1].level, LogLevel::Error);
    assert_eq!(records[1].scope, "root");
}

#[test]
fn sink_removal_and_service_withdrawal() {
    let ctx = Context::new();
    ctx.set_logger(Arc::new(yacordis::DiscardLogger));
    let logger_fiber = ctx
        .component(LoggerComponent, json!(null))
        .expect("install logger");

    let memory = Arc::new(MemorySink::new());
    let service = ctx.logger().expect("logger service");
    let sink_handle = service.add_arc_sink(memory.clone());

    ctx.log_info("kept");
    assert_eq!(memory.records().len(), 1);

    sink_handle.remove();
    ctx.log_info("dropped");
    assert_eq!(memory.records().len(), 1, "sink removed");

    logger_fiber.dispose();
    assert!(ctx.logger().is_none());
    ctx.log_info("no service, no crash");
}

#[test]
fn scoped_level_restores_on_fiber_unload() {
    let ctx = Context::new();
    ctx.set_logger(Arc::new(yacordis::DiscardLogger));
    ctx.component(LoggerComponent, json!(null))
        .expect("install logger");

    let mutator = ctx
        .component(
            |ctx: &Context, _: &Value| -> ComponentResult {
                ctx.set_scoped_level(Some(LogLevel::Debug))
                    .map_err(ComponentError::new)?;
                Ok(())
            },
            json!(null),
        )
        .expect("install mutator");

    let service = ctx.logger().expect("logger service");
    assert_eq!(service.level(), Some(LogLevel::Debug));

    mutator.dispose();
    assert_eq!(
        service.level(),
        None,
        "unloading the mutator restores the previous threshold"
    );
}

#[test]
fn scoped_sink_is_removed_with_its_fiber() {
    let ctx = Context::new();
    ctx.set_logger(Arc::new(yacordis::DiscardLogger));
    ctx.component(LoggerComponent, json!(null))
        .expect("install logger");

    let memory = Arc::new(MemorySink::new());
    let memory_for_body = memory.clone();
    let owner = ctx
        .component(
            move |ctx: &Context, _: &Value| -> ComponentResult {
                // The handle is dropped immediately: the scope release is
                // carried by the effect, not by the handle.
                let _ = ctx.add_scoped_arc_sink(memory_for_body.clone());
                Ok(())
            },
            json!(null),
        )
        .expect("install sink owner");
    assert!(owner.is_active());

    ctx.log_info("while installed");
    assert_eq!(memory.records().len(), 1);

    owner.dispose();
    ctx.log_info("after unload");
    assert_eq!(
        memory.records().len(),
        1,
        "the sink left with its fiber even though the handle was dropped"
    );
}

#[test]
fn nested_scoped_levels_restore_in_reverse_order() {
    let ctx = Context::new();
    ctx.set_logger(Arc::new(yacordis::DiscardLogger));
    ctx.component(LoggerComponent, json!(null))
        .expect("install logger");
    let service = ctx.logger().expect("logger service");

    let outer = ctx
        .component(
            |ctx: &Context, _: &Value| -> ComponentResult {
                ctx.set_scoped_level(Some(LogLevel::Warn))
                    .map_err(ComponentError::new)?;
                let inner = ctx.inject(Inject::none(), |ctx: &Context| {
                    ctx.set_scoped_level(Some(LogLevel::Debug))
                        .map_err(ComponentError::new)?;
                    Ok(())
                })?;
                assert!(inner.is_active());
                Ok(())
            },
            json!(null),
        )
        .expect("install outer");

    assert_eq!(service.level(), Some(LogLevel::Debug));
    outer.dispose();
    assert_eq!(
        service.level(),
        None,
        "inner restore runs first, then the outer one"
    );
}
