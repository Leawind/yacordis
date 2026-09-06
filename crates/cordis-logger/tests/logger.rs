//! Logger service tests: records, levels, scopes, sink removal.

use std::sync::Arc;

use cordis::prelude::*;
use cordis_logger::{LogLevel, LoggerContextExt, LoggerPlugin, MemorySink};
use serde_json::{Value, json};

#[test]
fn records_flow_to_sinks_with_plugin_scopes() {
    let ctx = Context::new();
    ctx.set_logger(Arc::new(cordis::DiscardLogger));
    ctx.plugin(LoggerPlugin, json!(null))
        .expect("install logger");

    let memory = Arc::new(MemorySink::new());
    let service = ctx.logger().expect("logger service");
    service.add_arc_sink(memory.clone());
    service.set_level(Some(LogLevel::Debug));

    let emitter = ctx
        .plugin(
            |ctx: &Context, _: &Value| -> PluginResult {
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
    emitter.dispose().expect("dispose");
}

#[test]
fn level_threshold_filters_records() {
    let ctx = Context::new();
    ctx.set_logger(Arc::new(cordis::DiscardLogger));
    ctx.plugin(LoggerPlugin, json!(null))
        .expect("install logger");

    let memory = Arc::new(MemorySink::new());
    let service = ctx.logger().expect("logger service");
    service.add_arc_sink(memory.clone());

    ctx.log_debug("hidden by default");
    assert_eq!(memory.records().len(), 0);

    ctx.log_warn("visible");
    assert_eq!(memory.records().len(), 1);

    // Root-context records are scoped `root`.
    ctx.log_error("boom");
    let records = memory.records();
    assert_eq!(records[1].level, LogLevel::Error);
    assert_eq!(records[1].scope, "root");
}

#[test]
fn sink_removal_and_service_withdrawal() {
    let ctx = Context::new();
    ctx.set_logger(Arc::new(cordis::DiscardLogger));
    let logger_fiber = ctx
        .plugin(LoggerPlugin, json!(null))
        .expect("install logger");

    let memory = Arc::new(MemorySink::new());
    let service = ctx.logger().expect("logger service");
    let sink_handle = service.add_arc_sink(memory.clone());

    ctx.log_info("kept");
    assert_eq!(memory.records().len(), 1);

    sink_handle.remove();
    ctx.log_info("dropped");
    assert_eq!(memory.records().len(), 1, "sink removed");

    // Withdrawing the logger plugin removes the service.
    logger_fiber.dispose().expect("dispose logger");
    assert!(ctx.logger().is_none());
    // Logging without the service is a no-op, not a panic.
    ctx.log_info("no service, no crash");
}
